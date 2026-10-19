//! Quick edit: a small text file changed in the preview and saved in place. The first save
//! sends the original to the trash, so Undo brings it back as it does a turned picture.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, FontId, Rect, Ui, pos2, vec2};

use crate::findbar::Finder;
use crate::theme::Palette;

/// Files up to this size are edited here; past it, typing would lag.
pub const MAX: u64 = 256 << 10;

pub struct Edit {
    pub path: PathBuf,
    /// The file a save writes: `path` with its links followed, so saving through a symlink
    /// changes what it points to and keeps the link.
    pub file: PathBuf,
    pub text: String,
    /// The file as read (modified, size): saving over a file changed since asks first.
    stamp: (Option<SystemTime>, u64),
    /// Every line break was CR LF: edited as LF and written back as CR LF. A file with
    /// both kinds keeps its CRs in the text, so each line is written back as it was.
    crlf: bool,
    /// What a Tab key inserts: a tab in a file indented with them, else four spaces.
    pub indent: &'static str,
    /// The text as last saved, to tell whether there is anything to save.
    saved: String,
    /// The original is in the trash already: the first save put it there.
    pub trashed: bool,
    lang: Option<&'static crate::syntax::Lang>,
    /// Counts changes to the text, for the matches found in it.
    pub rev: u64,
    /// The laid-out text, for the text and the matches it was made for.
    job: Option<(Arc<LayoutJob>, Vec<crate::findbar::Hit>, Option<usize>)>,
    /// Move the cursor to this byte range (a match) and scroll to it.
    pub select: Option<(usize, usize)>,
    /// The editor had the keyboard last frame.
    pub focused: bool,
}

pub enum SaveError {
    /// Changed on disk since it was opened.
    Changed,
    Failed(String),
}

impl Edit {
    pub fn open(path: &Path) -> Result<Edit, String> {
        let m = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if !m.is_file() || m.len() > MAX {
            return Err("Too big to edit here".into());
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let mut text = String::from_utf8(bytes).map_err(|_| "Not UTF-8 text".to_string())?;
        let crlf = text.contains("\r\n") && text.matches('\n').count() == text.matches("\r\n").count();
        if crlf {
            text = text.replace("\r\n", "\n");
        }
        let indent = if text.lines().any(|l| l.starts_with('\t')) { "\t" } else { "    " };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let file = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Edit { path: path.to_path_buf(), file, saved: text.clone(), text, stamp: (m.modified().ok(), m.len()), crlf, indent, trashed: false, lang: crate::syntax::lang_for(&name), rev: 0, job: None, select: Some((0, 0)), focused: false })
    }

    pub fn dirty(&self) -> bool {
        self.text != self.saved
    }

    /// Write the text over the file: beside it first, then renamed into place, keeping its
    /// permissions and tags. Returns whether the original went to the trash this time.
    pub fn save(&mut self, force: bool) -> Result<bool, SaveError> {
        let m = std::fs::metadata(&self.file).ok();
        if !force && m.as_ref().map(|m| (m.modified().ok(), m.len())) != Some(self.stamp) {
            return Err(SaveError::Changed);
        }
        let dir = self.file.parent().ok_or_else(|| SaveError::Failed("No folder".into()))?;
        let name = self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let part = crate::fsx::unique_dest(dir, &format!(".{name}.maia-part"));
        let body = if self.crlf { self.text.replace('\n', "\r\n") } else { self.text.clone() };
        let wrote = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&part)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            if let Some(m) = &m {
                std::fs::set_permissions(&part, m.permissions())?;
            }
            crate::ops::copy_xattrs(&self.file, &part);
            Ok::<(), std::io::Error>(())
        })();
        if let Err(e) = wrote {
            let _ = std::fs::remove_file(&part);
            return Err(SaveError::Failed(e.to_string()));
        }
        // The first save keeps the original in the trash, for Undo.
        let trash = !self.trashed && m.is_some() && crate::trashcan::delete(&self.file).is_ok();
        if let Err(e) = std::fs::rename(&part, &self.file) {
            if trash {
                if let Some(it) = crate::picfix::newest_trashed(&self.file) {
                    let _ = crate::trashcan::restore(&it);
                }
            }
            let _ = std::fs::remove_file(&part);
            return Err(SaveError::Failed(e.to_string()));
        }
        self.trashed |= trash;
        self.saved = self.text.clone();
        self.stamp = std::fs::metadata(&self.file).map(|m| (m.modified().ok(), m.len())).unwrap_or_default();
        Ok(trash)
    }

    /// Replace the current match, or all of them; returns how many were replaced.
    pub fn replace(&mut self, f: &mut Finder, all: bool) -> usize {
        let Some(with) = f.replace.clone() else { return 0 };
        let starts = line_starts(&self.text);
        let abs = |h: &crate::findbar::Hit| (starts[h.0 as usize] + h.1 as usize, starts[h.0 as usize] + h.2 as usize);
        let picked: Vec<(usize, usize)> = if all { f.hits.iter().map(abs).collect() } else { f.hits.get(f.cur).map(abs).into_iter().collect() };
        // From the end, so the ranges before stay where they are.
        for &(a, b) in picked.iter().rev() {
            self.text.replace_range(a..b, &with);
        }
        if let (false, Some(&(a, _))) = (all, picked.first()) {
            self.select = Some((a, a + with.len()));
        }
        self.rev += 1;
        picked.len()
    }
}

/// Where each line of `text` starts.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

/// The editor: the text with its colours and matches, line numbers beside it. Returns
/// whether the text changed.
#[allow(clippy::too_many_arguments)]
pub fn view(ui: &mut Ui, body: Rect, ed: &mut Edit, find: Option<&mut Finder>, numbers: bool, wrap: bool, size: f32, spacing: f32, pal: &Palette, id: u64) -> bool {
    let font = FontId::monospace(size);
    let lines: Vec<&str> = ed.text.split('\n').collect();
    let n_lines = lines.len();
    let (hits, cur): (&[crate::findbar::Hit], Option<usize>) = match find.filter(|f| !f.query.is_empty()) {
        Some(f) => {
            f.update(&lines, ed.rev);
            if std::mem::take(&mut f.reveal) {
                let starts = line_starts(&ed.text);
                ed.select = f.hits.get(f.cur).map(|h| (starts[h.0 as usize] + h.1 as usize, starts[h.0 as usize] + h.2 as usize));
            }
            (&f.hits, Some(f.cur))
        }
        None => (&[], None),
    };
    drop(lines);
    let gutter = if numbers { (n_lines.max(1).to_string().len() as f32) * size * 0.62 + 16.0 } else { 6.0 };
    let lang = ed.lang;
    let row_h = crate::inspector::row_height(ui, size, spacing);
    let mut cache = ed.job.take();
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let text = buf.as_str();
        let fresh = cache.as_ref().is_some_and(|(j, h, c)| *c == cur && h[..] == *hits && j.text == text);
        if !fresh {
            cache = Some((Arc::new(layout(text, lang, hits, cur, &font, row_h, pal)), hits.to_vec(), cur));
        }
        let mut job = (*cache.as_ref().unwrap().0).clone();
        job.wrap.max_width = if wrap { wrap_width } else { f32::INFINITY };
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let eid = egui::Id::new(("insp-edit", id));
    let mut changed = false;
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(body.shrink2(vec2(4.0, 4.0))));
    let area = if wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
    area.id_salt(("insp-edit-scroll", id)).auto_shrink([false, false]).show(&mut inner, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (gr, _) = ui.allocate_exact_size(vec2(gutter, 1.0), egui::Sense::hover());
            let te = egui::TextEdit::multiline(&mut ed.text)
                .id(eid)
                .code_editor()
                .frame(false)
                .margin(egui::Margin::ZERO)
                .desired_width(if wrap { ui.available_width() } else { f32::INFINITY })
                .desired_rows(1)
                .layouter(&mut layouter);
            let out = te.show(ui);
            ed.focused = ui.memory(|m| m.has_focus(eid));
            changed = out.response.changed();
            if changed {
                ed.rev += 1;
            }
            // A match picked: the cursor on it, scrolled into view.
            if let Some((a, b)) = ed.select.take() {
                let ch = |i: usize| CCursor::new(ed.text.get(..i).map_or(0, |t| t.chars().count()));
                let mut st = out.state.clone();
                st.cursor.set_char_range(Some(CCursorRange::two(ch(a), ch(b))));
                st.store(ui.ctx(), eid);
                let r = out.galley.pos_from_cursor(ch(a)).translate(out.galley_pos.to_vec2());
                ui.scroll_to_rect(r.expand2(vec2(40.0, 30.0)), Some(egui::Align::Center));
            }
            // Line numbers where each line starts, for the rows on screen.
            if numbers {
                let clip = ui.clip_rect();
                let p = ui.painter();
                let mut line = 1;
                let mut starts = true;
                for row in &out.galley.rows {
                    let y = out.galley_pos.y + row.rect().min.y;
                    if starts && y + row.rect().height() >= clip.top() && y <= clip.bottom() {
                        p.text(pos2(gr.left() + gutter - 10.0, y + row.rect().height() / 2.0), egui::Align2::RIGHT_CENTER, line.to_string(), font.clone(), pal.text_dim.gamma_multiply(0.7));
                    }
                    starts = row.ends_with_newline;
                    if starts {
                        line += 1;
                    }
                }
            }
        });
    });
    ed.job = cache;
    changed
}

/// The text laid out with its highlighting and the find bar's matches behind it.
fn layout(text: &str, lang: Option<&'static crate::syntax::Lang>, hits: &[crate::findbar::Hit], cur: Option<usize>, font: &FontId, row_h: f32, pal: &Palette) -> LayoutJob {
    let lines: Vec<String> = text.split('\n').map(String::from).collect();
    let spans = lang.map(|l| crate::syntax::highlight(l, &lines, lines.len()));
    let mut job = LayoutJob::default();
    // Rows as tall as the preview's, the text in their middle.
    let fmt = |c: Color32, bg: Color32| TextFormat { font_id: font.clone(), color: c, background: bg, line_height: Some(row_h), valign: egui::Align::Center, ..Default::default() };
    let mut h = 0;
    for (li, line) in lines.iter().enumerate() {
        // Every place the colour or the background changes on this line.
        let sp: &[(usize, usize, crate::syntax::Kind)] = spans.as_ref().and_then(|s| s.get(li)).map_or(&[], |s| &s[..]);
        let mut cuts: Vec<usize> = vec![0, line.len()];
        cuts.extend(sp.iter().flat_map(|s| [s.0, s.1]));
        while h < hits.len() && (hits[h].0 as usize) < li {
            h += 1;
        }
        let mine = hits[h..].iter().take_while(|x| x.0 as usize == li);
        cuts.extend(mine.flat_map(|x| [x.1 as usize, x.2 as usize]));
        cuts.retain(|&c| c <= line.len() && line.is_char_boundary(c));
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let col = sp.iter().find(|s| s.0 <= a && b <= s.1).map_or(pal.text, |s| crate::inspector::syntax_color(s.2, pal));
            let hit = hits[h..].iter().take_while(|x| x.0 as usize == li).position(|x| (x.1 as usize) <= a && b <= x.2 as usize);
            // The current match as a selection is drawn, the others tinted.
            let (col, bg) = match hit {
                Some(k) if cur == Some(h + k) => (pal.on_sel(), pal.selected),
                Some(_) => (col, pal.accent.gamma_multiply(0.25)),
                None => (col, Color32::TRANSPARENT),
            };
            job.append(&line[a..b], 0.0, fmt(col, bg));
        }
        if li + 1 < lines.len() {
            job.append("\n", 0.0, fmt(pal.text, Color32::TRANSPARENT));
        }
    }
    job
}
