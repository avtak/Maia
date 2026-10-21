//! Smart folders: saved searches, kept in the user data file and pinned in the sidebar.
//! Each is a folder to look in and a query in the filter's language (`src/query.rs`),
//! tested by the same code as the filter. Opening one runs the search again, so it is
//! never out of date: a flattened listing of the folder whose walk keeps only the files
//! that match, streaming in like any other.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;

use egui::{FontId, Id, Sense, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::fsx::{Entry, Walk};
use crate::popups::{self as pp, Popup};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartFolder {
    pub id: u64,
    pub name: String,
    pub root: PathBuf,
    /// What to find, as typed in the filter: `invoice type:pdfs modified<30d`.
    pub query: String,
    // The conditions smart folders kept before they kept a query, still read from older
    // files and searched as the query they amount to (`query_text`). Saving one from the
    // dialog writes them as its query.
    /// Part of the name, or a pattern with * and ? matched against the whole name.
    pub name_has: String,
    /// Any of these kinds (`KINDS`); none means every kind.
    pub kinds: Vec<String>,
    /// Changed within this many days.
    pub days: Option<u32>,
    /// Bigger than this many bytes.
    pub bigger: Option<u64>,
    /// A tag's view: every file and folder carrying it (the other conditions unused).
    pub tag: String,
}

/// The Timeline's id: a smart folder Maia keeps itself (saved ones count up from 1).
pub const TIMELINE: u64 = u64::MAX;

/// How far back the Timeline looks.
pub const TIMELINE_DAYS: u32 = 31;

/// Every file changed in the last month under the home folder.
pub fn timeline() -> SmartFolder {
    SmartFolder { id: TIMELINE, name: "Timeline".into(), root: crate::fsx::home(), days: Some(TIMELINE_DAYS), ..Default::default() }
}

/// A smart folder by id: one saved, or the Timeline.
pub fn by_id(saved: &[SmartFolder], id: u64) -> Option<SmartFolder> {
    if id == TIMELINE {
        return Some(timeline());
    }
    saved.iter().find(|s| s.id == id).cloned()
}

/// Ids of tag views: the top 16 bits set, then a hash of the tag. Saved smart folders
/// count up from 1, so the two never meet.
const TAG_IDS: u64 = 0xFFFE << 48;

pub fn tag_id(tag: &str) -> u64 {
    // FNV-1a over the lowercase name: the same in every run, so a saved session finds it.
    let h = tag.to_lowercase().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    TAG_IDS | (h & 0xFFFF_FFFF_FFFF)
}

/// Every file and folder in the home folder with this tag.
pub fn tag_view(tag: &str) -> SmartFolder {
    SmartFolder { id: tag_id(tag), name: tag.to_string(), root: crate::fsx::home(), tag: tag.to_string(), ..Default::default() }
}

/// A smart folder, the Timeline or a known tag's view, by id.
pub fn by_id_in(ud: &crate::config::UserData, id: u64) -> Option<SmartFolder> {
    if id & (0xFFFF << 48) == TAG_IDS {
        return ud.tags.iter().find(|t| tag_id(t) == id).map(|t| tag_view(t));
    }
    by_id(&ud.smart, id)
}

/// The Timeline's view: newest first, in the date groups (today, yesterday, this week…).
pub fn timeline_view(mut v: crate::config::ViewSettings) -> crate::config::ViewSettings {
    v.sort = crate::config::SortKey::Modified;
    v.desc = true;
    v.group = crate::config::GroupBy::Modified;
    v
}

/// The kinds older smart folders could ask for, by their stored names.
const KINDS: [&str; 7] = ["pictures", "documents", "pdfs", "videos", "music", "archives", "code"];

/// `*` for any run of characters, `?` for one; both sides already lowercase. Bytes do for
/// plain ASCII, without making a list of characters per name.
pub fn glob<T: Copy + PartialEq + From<u8>>(pat: &[T], name: &[T]) -> bool {
    let (any, one) = (T::from(b'*'), T::from(b'?'));
    let (mut p, mut n) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pat.len() && (pat[p] == one || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == any {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == any)
}

impl SmartFolder {
    /// The query searched for: its own, after what an older smart folder kept, each as
    /// the filter would say it.
    pub fn query_text(&self) -> String {
        let mut v: Vec<String> = vec![];
        let has = self.name_has.trim().replace('"', "");
        if !has.is_empty() {
            v.push(if has.contains(|c: char| c.is_whitespace() || "|()".contains(c)) { format!("name:\"{has}\"") } else { format!("name:{has}") });
        }
        // A kind a later version added is left out, as this version always ignored it.
        let kinds: Vec<&str> = self.kinds.iter().map(String::as_str).filter(|k| KINDS.contains(k)).collect();
        if !kinds.is_empty() {
            v.push(format!("type:{}", kinds.join(",")));
        }
        if let Some(d) = self.days {
            v.push(format!("modified<{d}d"));
        }
        if let Some(b) = self.bigger {
            let unit = [(1u64 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")].into_iter().find(|u| b >= u.0 && b % u.0 == 0);
            v.push(unit.map_or_else(|| format!("size>{b}"), |(n, u)| format!("size>{}{u}", b / n)));
        }
        let q = self.query.trim();
        if !q.is_empty() {
            // Its alternatives kept together, next to the older conditions.
            let ors = !v.is_empty() && (q.contains('|') || q.split_whitespace().any(|w| w == "OR"));
            v.push(if ors { format!("({q})") } else { q.to_string() });
        }
        v.join(" ")
    }

    /// The folder and the query, for the sidebar's tooltip.
    pub fn summary(&self) -> String {
        let q = self.query_text();
        if q.is_empty() { pp::home_short(&self.root) } else { format!("{} · “{q}”", pp::home_short(&self.root)) }
    }
}

/// The files under the smart folder's folder that match it, streamed in batches as a
/// flattened listing's walk is. Not looked in: hidden folders, build output and
/// installed packages, links, other file systems.
pub fn walk(sf: SmartFolder, ctx: egui::Context) -> Walk {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let truncated = Arc::new(AtomicBool::new(false));
    let (c2, d2, t2) = (cancel.clone(), done.clone(), truncated.clone());
    let _ = std::thread::Builder::new().name("smart folder".into()).spawn(move || {
        if crate::fsx::is_virtual_fs(&sf.root) {
        } else if !sf.tag.is_empty() {
            let tx = Mutex::new(tx);
            let send = |batch| tx.lock().unwrap_or_else(|e| e.into_inner()).send(batch).is_ok() && { ctx.request_repaint(); true };
            crate::tags::search(&sf.root, &sf.tag, &send, &c2, &t2);
        } else {
            search(&sf, &mut |batch| tx.send(batch).is_ok() && { ctx.request_repaint(); true }, &c2, &t2);
        }
        d2.store(true, Ordering::Relaxed);
        ctx.request_repaint();
    });
    Walk { rx, cancel, done, truncated }
}

/// The search itself: `send` takes each batch and says whether to go on.
pub fn search(sf: &SmartFolder, send: &mut dyn FnMut(Vec<Entry>) -> bool, cancel: &AtomicBool, truncated: &AtomicBool) {
    use std::os::unix::fs::MetadataExt;
    let text = sf.query_text();
    if text.contains("owner:") || text.contains("user:") {
        let _ = crate::query::users(true);
    }
    let f = crate::query::parse(&text, SystemTime::now());
    let stats = std::collections::HashMap::new();
    // First what the name and path tell, so most files are never looked at; tags are
    // read from the files, this being a worker.
    let by_name = crate::query::Cx { stats: &stats, text: crate::query::Text::Full, meta: false, read_tags: true };
    let full = crate::query::Cx { meta: true, ..by_name };
    let root = &sf.root;
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return };
    let rb = root.as_os_str().as_encoded_bytes();
    let rel_at = (rb.len() + usize::from(rb.last() != Some(&b'/'))).clamp(1, u16::MAX as usize) as u16;
    let mut stack = vec![root.clone()];
    let mut batch = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut flushed = std::time::Instant::now();
    while let Some(d) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
        let has = |n: &str| kids.iter().any(|k| k.0 == n);
        for (name, ft, path) in &kids {
            if name.starts_with('.') || ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if crate::buildclean::kind_by_name(name, &has).is_none() && std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == dev) {
                    stack.push(path.clone());
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let mut probe = Entry::placeholder(path.clone(), name.clone(), false);
            probe.rel_at = rel_at;
            if !f.may(&probe, &by_name) {
                continue;
            }
            let Some(mut e) = Entry::from_path(path) else { continue };
            e.rel_at = rel_at;
            if f.score(&e, &full).is_none() {
                continue;
            }
            batch.push(e);
            total += 1;
            if total >= crate::fsx::WALK_LIMIT {
                truncated.store(true, Ordering::Relaxed);
                send(std::mem::take(&mut batch));
                return;
            }
        }
        if !batch.is_empty() && (batch.len() >= 4096 || flushed.elapsed().as_millis() > 50) {
            if !send(std::mem::take(&mut batch)) {
                return;
            }
            flushed = std::time::Instant::now();
        }
    }
    if !batch.is_empty() {
        send(batch);
    }
}

/// A smart folder from what a tab's filter is searching for: its folder and the whole
/// filter, named after the words in it.
pub fn from_filter(folder: &Path, filter: &str) -> SmartFolder {
    let words = crate::query::parse(filter, SystemTime::now()).name.trim().to_string();
    let name = if words.is_empty() { folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Search".into()) } else { words };
    SmartFolder { name, root: folder.to_path_buf(), query: filter.trim().to_string(), ..Default::default() }
}

// ------------------------------------------------------------------ the dialog

pub struct Edit {
    /// The smart folder changed; None for a new one.
    pub id: Option<u64>,
    pub name: String,
    pub root: String,
    pub query: String,
    focus: bool,
    pub error: Option<String>,
}

pub fn edit(sf: &SmartFolder, existing: bool) -> Popup {
    Popup::SmartEdit(Edit {
        id: existing.then_some(sf.id),
        name: sf.name.clone(),
        root: pp::home_short(&sf.root),
        query: sf.query_text(),
        focus: true,
        error: None,
    })
}

impl Edit {
    /// The smart folder the fields describe, or what is wrong with them.
    pub fn build(&self) -> Result<SmartFolder, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("Give it a name".into());
        }
        let root = crate::fsx::expand_path(&self.root);
        if !root.is_dir() {
            return Err(format!("There is no folder at {}", self.root.trim()));
        }
        let query = self.query.trim().to_string();
        if let Some(&(a, b)) = crate::query::parse(&query, SystemTime::now()).bad.first() {
            return Err(format!("Find: {} is not a regular expression", &query[a..b]));
        }
        Ok(SmartFolder { id: self.id.unwrap_or(0), name, root, query, ..Default::default() })
    }
}

/// New or edit: the name, the folder and the conditions; Save keeps it in the sidebar.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut Edit) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let mut save = k.enter;
    let mut cancel = false;
    let title = match st.id {
        None => "New smart folder".to_string(),
        Some(_) => format!("Edit “{}”", st.name.trim()),
    };
    pp::centered(ctx, "smart folder", 600.0, &pal, |ui| {
        pp::dialog_title(ui, &title, None, &pal);
        ui.add_space(8.0);
        let label_w = (fs * 6.2).round();
        let label = |ui: &mut egui::Ui, t: &str| {
            let (r, _) = ui.allocate_exact_size(vec2(label_w, fs * 1.6), Sense::hover());
            ui.painter().text(pos2(r.left(), r.center().y), egui::Align2::LEFT_CENTER, t, FontId::proportional(fs - 0.5), pal.text_dim);
        };
        let field = |ui: &mut egui::Ui, text: &mut String, id: &'static str, hint: &str, w: f32| {
            let r = ui.add(egui::TextEdit::singleline(text).id(Id::new(("smart folder", id))).hint_text(egui::RichText::new(hint).color(pal.text_dim)).desired_width(w).margin(egui::Margin::symmetric(6, 3)));
            crate::ui::note_control(ui, r.rect, id);
            r
        };
        let wide = (ui.available_width() - label_w - 12.0).max(120.0);
        let changed = |r: &egui::Response| r.changed();
        let mut edited = false;
        ui.horizontal(|ui| {
            label(ui, "Name");
            let r = field(ui, &mut st.name, "smart name", "", wide);
            if st.focus {
                r.request_focus();
                st.focus = false;
            }
            edited |= changed(&r);
        });
        ui.horizontal(|ui| {
            label(ui, "Look in");
            edited |= changed(&field(ui, &mut st.root, "smart root", "~/Documents", wide));
        });
        ui.horizontal(|ui| {
            label(ui, "Find");
            // Marked as the filter marks it, conditions in the accent.
            let (text, accent, danger) = (pal.text, pal.accent, pal.danger);
            // On one line that scrolls, like any single-line field.
            let mut marks = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                let job = crate::query::job(buf.as_str(), egui::TextStyle::Body.resolve(ui.style()), text, accent, danger);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let r = ui.add(egui::TextEdit::singleline(&mut st.query).id(Id::new(("smart folder", "smart query"))).hint_text(egui::RichText::new("invoice type:pdfs modified<30d").color(pal.text_dim)).desired_width(wide).margin(egui::Margin::symmetric(6, 3)).layouter(&mut marks));
            crate::ui::note_control(ui, r.rect, "smart query");
            edited |= changed(&r);
        });
        if edited {
            st.error = None;
        }
        if let Some(e) = &st.error {
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(e).size(small).color(pal.danger)).wrap());
        }
        pp::hints(ui, &[("Enter", "save"), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            let b = pp::primary_button(ui, "Save", !st.name.trim().is_empty(), &pal);
            crate::ui::note_control(ui, b.rect, "smart save");
            save |= b.clicked();
            let c = pp::plain_button(ui, "Cancel", &pal);
            crate::ui::note_control(ui, c.rect, "smart cancel");
            cancel = c.clicked();
        });
    });
    if cancel {
        return false;
    }
    if save {
        match st.build() {
            Ok(sf) => {
                app.save_smart(sf, st.id.is_none());
                return false;
            }
            Err(e) => st.error = Some(e),
        }
    }
    true
}

// ------------------------------------------------------------------ keeping them

impl App {
    /// Show smart folder `id` in `panel`'s tab, or in a new tab there.
    pub fn open_smart(&mut self, panel: u64, id: u64, new_tab: bool, focus: bool) {
        let Some(sf) = by_id_in(&self.ud, id) else { return };
        if !sf.root.is_dir() {
            self.toast(format!("{} is no longer there", pp::home_short(&sf.root)));
            return;
        }
        let timeline = id == TIMELINE;
        if new_tab {
            let mut t = self.make_tab(sf.root.clone());
            let view = timeline.then(|| (timeline_view(t.settings.clone()), None));
            t.open_smart(sf, view);
            if let Some(p) = self.panels.get_mut(&panel) {
                let at = (p.active + 1).min(p.tabs.len());
                p.tabs.insert(at, t);
                if focus {
                    p.active = at;
                }
            }
            return;
        }
        let s = App::nav_view(&self.ud, &sf.root);
        let Some(p) = self.panels.get_mut(&panel) else { return };
        let s = if timeline { Some((timeline_view(s.map(|x| x.0).unwrap_or_else(|| p.tab().settings.clone())), None)) } else { s };
        let old = p.path();
        p.tab_mut().open_smart(sf, s);
        if let Some(i) = p.inspector.as_mut() {
            i.invalidate();
        }
        self.track_leave(&old);
    }

    /// Keep a smart folder: a new one goes to the end of the list and opens here; a
    /// changed one is searched again wherever it shows.
    pub fn save_smart(&mut self, mut sf: SmartFolder, new: bool) {
        if new {
            sf.id = self.ud.smart.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            let id = sf.id;
            self.ud.smart.push(sf);
            self.ud_dirty = true;
            let panel = self.active;
            self.open_smart(panel, id, false, true);
            return;
        }
        let Some(slot) = self.ud.smart.iter_mut().find(|s| s.id == sf.id) else { return };
        *slot = sf.clone();
        self.ud_dirty = true;
        for p in self.panels.values_mut() {
            for t in p.tabs.iter_mut().filter(|t| t.smart.as_ref().is_some_and(|s| s.id == sf.id)) {
                if t.path == sf.root {
                    t.smart = Some(sf.clone());
                    t.reload();
                } else {
                    t.open_smart(sf.clone(), None);
                }
            }
        }
    }

    pub fn rename_smart(&mut self, id: u64, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(s) = self.ud.smart.iter_mut().find(|s| s.id == id) {
            s.name = name.clone();
            self.ud_dirty = true;
        }
        for p in self.panels.values_mut() {
            for t in p.tabs.iter_mut() {
                if let Some(s) = t.smart.as_mut().filter(|s| s.id == id) {
                    s.name = name.clone();
                }
            }
        }
    }

    /// Take a smart folder out of the sidebar; tabs showing it show its folder instead.
    pub fn remove_smart(&mut self, id: u64) {
        self.ud.smart.retain(|s| s.id != id);
        self.ud_dirty = true;
        for p in self.panels.values_mut() {
            for t in p.tabs.iter_mut().filter(|t| t.smart.as_ref().is_some_and(|s| s.id == id)) {
                t.set_flatten(false);
            }
        }
    }
}
