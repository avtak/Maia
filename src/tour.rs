//! The welcome tour, shown once on a first start, and What's New, shown once after an
//! update. A few pages, each a feature with a short film of it at work; the tour's first
//! page holds the choices most people make on arrival. Both close with Esc or a click
//! outside at any page, and come back only from the palette or Options.
//!
//! The films (assets/tour) are made by tools/tour-media.sh from the simulator, in the
//! Graphite theme with Google Sans Flex and Google Sans Code, a few states each: the
//! frames in between cost nearly a whole picture apiece, which the binary has no room
//! for. They are decoded on a thread of their own, a frame ahead, and only while shown.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use egui::{Align2, CornerRadius, FontId, Id, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use crate::app::App;
use crate::commands::Cmd;
use crate::popups::{self as pp, Opt, Popup};
use crate::theme::{Palette, ThemeName};

/// The version What's New describes. A start that has seen an older one shows it.
pub const NEWS_VERSION: &str = "0.5.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Welcome,
    News,
}

/// What a page shows besides its words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body {
    /// A film of the feature.
    Film(&'static [u8]),
    /// The choices made on arrival.
    Setup,
    /// Everything else the version added, in groups.
    List,
}

pub struct Page {
    pub title: &'static str,
    pub text: &'static str,
    pub body: Body,
    /// The commands that reach it, shown with their keys as the user has them.
    pub keys: &'static [Cmd],
}

const GOTO: &[u8] = include_bytes!("../assets/tour/goto.webp");
const PALETTE: &[u8] = include_bytes!("../assets/tour/palette.webp");
const SPLIT: &[u8] = include_bytes!("../assets/tour/split.webp");
const PREVIEW: &[u8] = include_bytes!("../assets/tour/preview.webp");
const GIT: &[u8] = include_bytes!("../assets/tour/git.webp");
const PICKER: &[u8] = include_bytes!("../assets/tour/picker.webp");
const COLUMNS: &[u8] = include_bytes!("../assets/tour/columns.webp");
const FILTER: &[u8] = include_bytes!("../assets/tour/filter.webp");
const GITSEARCH: &[u8] = include_bytes!("../assets/tour/gitsearch.webp");
const SORTING: &[u8] = include_bytes!("../assets/tour/sorting.webp");

pub const WELCOME: &[Page] = &[
    Page {
        title: "Welcome to Maia",
        text: "A file manager to drive from the keyboard, with the mouse there when you want it. A few choices first; nothing changes unless you pick it.",
        body: Body::Setup,
        keys: &[],
    },
    Page {
        title: "Go anywhere by typing",
        text: "Go To finds a folder by a few letters of its name, from the places you have been, or takes a path and completes it. Typing in a folder filters it.",
        body: Body::Film(GOTO),
        keys: &[Cmd::GoTo, Cmd::AddressBar],
    },
    Page {
        title: "Every command by its name",
        text: "The palette lists every command with its keys. Run one from there, star it for the Quick menu, or give it keys of your own.",
        body: Body::Film(PALETTE),
        keys: &[Cmd::Palette],
    },
    Page {
        title: "Two folders side by side",
        text: "Split the window and copy or move between the panels. Copies run in the background, and Undo takes them back.",
        body: Body::Film(SPLIT),
        keys: &[Cmd::SplitRight, Cmd::FocusNextPanel, Cmd::Undo],
    },
    Page {
        title: "Look inside without opening",
        text: "The inspector shows the file under the cursor beside the list: Markdown, tables, code, pictures, PDFs, sound, video and archives.",
        body: Body::Film(PREVIEW),
        keys: &[Cmd::ToggleInspector],
    },
    Page {
        title: "Git where your files are",
        text: "In a repository, the branch in the status bar opens the history, branches and stash, with pull and push. The changed files can be listed alone.",
        body: Body::Film(GIT),
        keys: &[Cmd::OnlyChanged, Cmd::RecentProjects],
    },
];

pub const NEWS: &[Page] = &[
    Page {
        title: "Maia picks your files",
        text: "Apps that ask the desktop for a file can get Maia instead of their own dialog, with the filter, previews and every key you know. Turn it on in Options, Startup & system.",
        body: Body::Film(PICKER),
        keys: &[Cmd::Options],
    },
    Page {
        title: "A column for each folder",
        text: "Columns lays out the way down side by side, one folder to a column. Right goes into a folder and Left back out; everything else works as it does in the list.",
        body: Body::Film(COLUMNS),
        keys: &[Cmd::ViewColumns],
    },
    Page {
        title: "Say what you want",
        text: "The filter takes OR, NOT, type:, ext:, owner: and patterns, as in pdf | ext:md -recipes. Save As Smart Folder keeps all of it in the sidebar.",
        body: Body::Film(FILTER),
        keys: &[Cmd::FocusFilter, Cmd::SaveSmart],
    },
    Page {
        title: "Search the history",
        text: "Ctrl+F over the git history finds commits by their words, author:, path: or date. A branch's menu compares it with yours, or merges it in.",
        body: Body::Film(GITSEARCH),
        keys: &[Cmd::GitHistory, Cmd::GitSearch],
    },
    Page {
        title: "Sort photos as you look",
        text: "In the viewer, Delete puts a picture in the trash, R and L turn it, Ctrl+1 to Ctrl+7 give it a colour, T any tag, and C holds it beside the next one.",
        body: Body::Film(SORTING),
        keys: &[Cmd::ViewPictures],
    },
    Page { title: "Also new", text: "", body: Body::List, keys: &[] },
];

/// Everything else 0.5.0 added: (group, [(name, one line)]).
pub const ALSO: &[(&str, &[(&str, &str)])] = &[
    (
        "For everyone",
        &[
            ("Copies take turns", "Copies to and from one disk wait for each other instead of slowing each other down; Start now runs one at once."),
            ("Check copies", "Every copy read back from the disk and compared with the original (Options, Files & folders)."),
            ("Redo", "Ctrl+Shift+Z or Ctrl+Y does again what Undo took back, and both outlast a restart."),
            ("Undo across drives", "A move to another drive is undone by moving it back."),
            ("Select and copy text", "In every preview: text, Markdown, tables, data, logs and hex."),
            ("Tags in the sidebar", "Every file with a tag in one place, a Tags column, sort and group by tag, and a tag renamed or removed across your home."),
            ("Date taken, size and length", "Date Taken, Dimensions and Duration columns; photos grouped by the month they were taken."),
            ("Change archives", "Add, rename and delete inside zip, 7z and tar files; compress with a name, a level and real progress."),
        ],
    ),
    (
        "For power users",
        &[
            ("Copy To and Move To", "Shift+F5 and Shift+F6 pick the folder in Go To, the recent ones first."),
            ("Select Matching", "A pattern such as *.jpg or IMG_*.{png,jpg} selects what it names."),
            ("Packages and lone files", ".deb and .rpm packages, and a lone .gz, .xz, .bz2 or .zst file, open like folders."),
        ],
    ),
    (
        "For developers",
        &[
            ("Several terminals", "Ctrl+Shift+T in the terminal opens another shell; Ctrl+Shift+F searches what scrolled past."),
            ("Links in the terminal", "Ctrl+click a URL or a file:line to open it; vim, less and tmux get the mouse."),
        ],
    ),
];

pub fn pages(kind: Kind) -> &'static [Page] {
    match kind {
        Kind::Welcome => WELCOME,
        Kind::News => NEWS,
    }
}

pub struct State {
    pub kind: Kind,
    pub page: usize,
    /// Shown by itself at start: closing it says where it can be found again.
    pub auto: bool,
    film: Option<Film>,
    /// When the page was turned (for its fade in).
    turned: Option<f64>,
}

pub fn open(kind: Kind, auto: bool) -> Popup {
    Popup::Tour(Box::new(State { kind, page: 0, auto, film: None, turned: None }))
}

impl State {
    pub fn pages(&self) -> &'static [Page] {
        pages(self.kind)
    }
    /// The film on screen has played to its end (or shows its last state, with
    /// animations off).
    pub fn film_done(&self) -> bool {
        self.film.as_ref().is_some_and(|f| f.done && f.tex.is_some())
    }
    /// The film's picture is on screen.
    pub fn film_shown(&self) -> bool {
        self.film.as_ref().is_some_and(|f| f.tex.is_some())
    }
}

// ------------------------------------------------------------------ first start

#[cfg(test)]
thread_local! {
    /// Test builds show neither by themselves unless a test asks: every simulation
    /// starts in a fresh home, which would otherwise be a first start each time.
    pub static AUTO: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A version as numbers ("0.3.0" → [0, 3, 0]); missing parts are 0, anything after a
/// number (a "-rc1") is ignored.
pub fn version(v: &str) -> [u32; 3] {
    let mut out = [0; 3];
    for (i, p) in v.trim().trim_start_matches('v').split('.').take(3).enumerate() {
        out[i] = p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0);
    }
    out
}

/// What a start shows: the tour when nothing of Maia's is on disk yet, What's New when
/// the newest seen is older than the notes (a 0.2 home had no record of it: older), and
/// nothing otherwise.
pub fn at_start_kind(first: bool, seen: &str, news: &str) -> Option<Kind> {
    if first {
        Some(Kind::Welcome)
    } else if seen.is_empty() || version(seen) < version(news) {
        Some(Kind::News)
    } else {
        None
    }
}

/// On starting: show the tour or What's New when due, and remember this version as
/// seen, written at once so that neither comes back by itself.
pub fn at_start(app: &mut App, first: bool) {
    #[cfg(test)]
    if !AUTO.with(|a| a.get()) {
        return;
    }
    let now = env!("CARGO_PKG_VERSION");
    if version(&app.ud.seen_version) >= version(now) && !first {
        return;
    }
    let kind = at_start_kind(first, &app.ud.seen_version, NEWS_VERSION);
    app.ud.seen_version = now.to_string();
    app.ud_dirty = true;
    app.save_userdata();
    if let Some(k) = kind {
        app.popup = open(k, true);
    }
}

// ------------------------------------------------------------------ films

/// One frame of a film, decoded.
pub(crate) struct Frame {
    pub(crate) img: egui::ColorImage,
    pub(crate) ms: u32,
}

/// A film playing: its frames come from a thread one ahead, held for their time.
struct Film {
    bytes: &'static [u8],
    rx: Receiver<Frame>,
    /// Set while the frame due has not come yet: the thread wakes the window with it.
    waiting: Arc<AtomicBool>,
    tex: Option<egui::TextureHandle>,
    /// Width over height, once known.
    aspect: f32,
    /// When the frame on screen is over.
    until: f64,
    done: bool,
}

/// How many times a film plays before it rests on its last state.
pub(crate) const PLAYS: u32 = 3;

impl Film {
    fn start(bytes: &'static [u8], still: bool, ctx: &egui::Context) -> Film {
        let (tx, rx) = sync_channel(1);
        let waiting = Arc::new(AtomicBool::new(true));
        let (w, c) = (waiting.clone(), ctx.clone());
        std::thread::spawn(move || play(bytes, still, tx, w, c));
        Film { bytes, rx, waiting, tex: None, aspect: 1.6, until: 0.0, done: false }
    }

    /// Show the next frame when the one on screen is over.
    fn pump(&mut self, ctx: &egui::Context) {
        if self.done {
            return;
        }
        let now = ctx.input(|i| i.time);
        if self.tex.is_some() && now < self.until {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(self.until - now));
            return;
        }
        let f = match self.rx.try_recv() {
            Ok(f) => f,
            Err(TryRecvError::Empty) => {
                // Asked for before looking again: a frame landing in between still wakes us.
                self.waiting.store(true, Ordering::Release);
                match self.rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => return,
                    Err(TryRecvError::Disconnected) => {
                        self.done = true;
                        return;
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                return;
            }
        };
        self.aspect = f.img.size[0] as f32 / f.img.size[1].max(1) as f32;
        let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
        match &mut self.tex {
            Some(t) => t.set(f.img, opts),
            None => self.tex = Some(ctx.load_texture("tour film", f.img, opts)),
        }
        self.until = now + f.ms as f64 / 1000.0;
        if f.ms > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(f.ms as u64));
        }
    }
}

/// Decode `bytes` frame by frame, `PLAYS` times over (only the last frame when `still`),
/// handing each over when the one before has been taken.
pub(crate) fn play(bytes: &'static [u8], still: bool, tx: SyncSender<Frame>, waiting: Arc<AtomicBool>, ctx: egui::Context) {
    let Ok(mut d) = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)) else { return };
    let (w, h) = d.dimensions();
    let (w, h) = (w as usize, h as usize);
    let alpha = d.has_alpha();
    let Some(len) = d.output_buffer_size() else { return };
    let mut buf = vec![0u8; len];
    let n = if d.is_animated() { d.num_frames().max(1) } else { 1 };
    for round in 0..PLAYS {
        for i in 0..n {
            let ms = if d.is_animated() {
                match d.read_frame(&mut buf) {
                    Ok(ms) => ms,
                    Err(_) => return,
                }
            } else {
                if d.read_image(&mut buf).is_err() {
                    return;
                }
                0
            };
            if still && i + 1 < n {
                continue;
            }
            let img = if alpha { egui::ColorImage::from_rgba_unmultiplied([w, h], &buf) } else { egui::ColorImage::from_rgb([w, h], &buf) };
            // The last state of the last play stays: no time of its own.
            let last = still || (round + 1 == PLAYS && i + 1 == n);
            if tx.send(Frame { img, ms: if last { 0 } else { ms } }).is_err() {
                return;
            }
            if waiting.swap(false, Ordering::AcqRel) {
                ctx.request_repaint();
            }
        }
        if still || n == 1 {
            return;
        }
        d.reset_animation();
    }
}

// ------------------------------------------------------------------ drawing

/// The choices on the first page, by the Options page they live on.
const SETUP: &[(&str, &[Opt])] = &[
    ("Appearance", &[Opt::FollowSystem, Opt::FontSize, Opt::Animations]),
    ("Files & folders", &[Opt::ShowHidden, Opt::Thumbnails]),
    ("Mouse & keyboard", &[Opt::SingleClick]),
    ("Search", &[Opt::Meaning]),
    ("Startup & system", &[Opt::DefaultFm]),
];

/// The themes offered on the first page: dark and light ones people know.
const THEMES: [ThemeName; 6] = [ThemeName::Midnight, ThemeName::Graphite, ThemeName::TokyoNight, ThemeName::Mocha, ThemeName::Paper, ThemeName::Latte];

pub fn draw(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let pages = st.pages();
    let n = pages.len();
    st.page = st.page.min(n - 1);
    // A text field with the keyboard (Options' own, in a setup row) keeps its arrows.
    let typing = ctx.memory(|m| m.focused()).is_some_and(|f| egui::TextEdit::load_state(ctx, f).is_some());
    let (esc, left, right, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::Escape),
            !typing && i.consume_key(Modifiers::NONE, Key::ArrowLeft),
            !typing && i.consume_key(Modifiers::NONE, Key::ArrowRight),
            !typing && i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    if esc {
        return closed(app, st);
    }
    let mut go: Option<usize> = None;
    let mut close = false;
    if left && st.page > 0 {
        go = Some(st.page - 1);
    }
    if right || enter {
        if st.page + 1 < n {
            go = Some(st.page + 1);
        } else if enter {
            close = true;
        }
    }
    let page = &pages[st.page];
    let now = ctx.input(|i| i.time);
    let turned = *st.turned.get_or_insert(now);
    // The page's film: started when the page is, decoded off the window's thread.
    if let Body::Film(bytes) = page.body {
        if st.film.as_ref().is_none_or(|f| !std::ptr::eq(f.bytes, bytes)) {
            st.film = Some(Film::start(bytes, !app.cfg.animations, ctx));
        }
    } else {
        st.film = None;
    }
    if let Some(f) = &mut st.film {
        f.pump(ctx);
    }
    // A new page fades in, as other surfaces of the window do.
    let fade = if app.cfg.animations { (((now - turned) / 0.16) as f32).clamp(0.0, 1.0) } else { 1.0 };
    if fade < 1.0 {
        ctx.request_repaint();
    }
    let screen = ctx.content_rect();
    let w = 556f32.min(screen.width() - 24.0).round();
    // Tall enough for a film at one pixel for one and its words at this font size.
    let fs = app.cfg.font_size;
    let h = (528.0 + (fs - 12.0).max(0.0) * 10.0).min(screen.height() - 24.0).round();
    let at = pos2((screen.center().x - w / 2.0).round(), (screen.top() + ((screen.height() - h) / 2.0).clamp(12.0, 70.0)).round());
    pp::note_layer(Id::new("tour"));
    let mut replay = false;
    let area = egui::Area::new(Id::new("tour")).order(egui::Order::Foreground).fixed_pos(at).show(ctx, |ui| {
        pp::popup_frame(&pal).show(ui, |ui| {
            let (_, inner) = ui.allocate_space(vec2(w - 16.0, h - 16.0));
            // The head: what this is, and a close button.
            let head = Rect::from_min_size(inner.min, vec2(inner.width(), 24.0));
            // What's New says which version; the tour's first title says the rest.
            let label = match st.kind {
                Kind::Welcome => String::new(),
                Kind::News => format!("New in Maia {NEWS_VERSION}"),
            };
            ui.painter().text(pos2(head.left() + 4.0, head.center().y), Align2::LEFT_CENTER, label, FontId::proportional(11.5), pal.text_dim);
            let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(head).layout(egui::Layout::right_to_left(egui::Align::Center)));
            if crate::ui::glyph_button_as(&mut hui, crate::theme::Glyph::Close, "Close (Esc)", true, &pal, "tour close").clicked() {
                close = true;
            }
            // The foot: where you are, the keys, and the way on.
            let foot = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 28.0), inner.max);
            let dots_w = dots(ui, foot, n, st.page, &pal, &mut go);
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(foot).layout(egui::Layout::right_to_left(egui::Align::Center)));
            fui.spacing_mut().item_spacing.x = 8.0;
            let last = st.page + 1 == n;
            let b = pp::primary_button(&mut fui, if last { "Done" } else { "Next" }, true, &pal);
            crate::ui::note_control(&fui, b.rect, "tour next");
            if b.clicked() {
                if last {
                    close = true;
                } else {
                    go = Some(st.page + 1);
                }
            }
            if st.page > 0 {
                let b = pp::plain_button(&mut fui, "Back", &pal);
                crate::ui::note_control(&fui, b.rect, "tour back");
                if b.clicked() {
                    go = Some(st.page - 1);
                }
            }
            let hints = Rect::from_min_max(pos2(foot.left() + dots_w + 16.0, foot.top()), pos2(fui.min_rect().left() - 12.0, foot.bottom()));
            if hints.width() > 40.0 {
                pp::hint_row(ui, hints, &[("←→", "pages"), ("Esc", "close")], &pal);
            }
            // The page itself, between them.
            let body = Rect::from_min_max(pos2(inner.left(), head.bottom() + 6.0), pos2(inner.right(), foot.top() - 10.0));
            let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(egui::Align::Min)));
            bui.set_clip_rect(body.intersect(ui.clip_rect()));
            bui.multiply_opacity(fade * (2.0 - fade));
            match page.body {
                Body::Film(_) => replay = film_page(&mut bui, app, st, body),
                Body::Setup => setup_page(&mut bui, app, page, body),
                Body::List => list_page(&mut bui, app, body),
            }
        });
    });
    let rect = area.response.rect;
    crate::ui::note_popup(rect);
    if replay {
        st.film = None;
    }
    if let Some(p) = go.filter(|p| *p != st.page && *p < n) {
        st.page = p;
        st.turned = Some(now);
        st.film = None;
        ctx.request_repaint();
    }
    // A click outside closes it, as it does other dialogs, unless it lands in a menu
    // one of its controls opened.
    if close || (!egui::Popup::is_any_open(ctx) && pp::clicked_outside(ctx, rect)) {
        return closed(app, st);
    }
    true
}

/// Closing: the first time by itself, a word on where to find it again.
fn closed(app: &mut App, st: &State) -> bool {
    if st.auto {
        app.toast(match st.kind {
            Kind::Welcome => "The tour is in the command palette: Welcome Tour",
            Kind::News => "This is in the command palette: What's New in Maia",
        });
    }
    false
}

/// The pages as dots along the foot's left end; a click goes to one. Returns their width.
fn dots(ui: &mut Ui, foot: Rect, n: usize, page: usize, pal: &Palette, go: &mut Option<usize>) -> f32 {
    let step = 14.0;
    for i in 0..n {
        let c = pos2(foot.left() + 8.0 + i as f32 * step, foot.center().y);
        let r = Rect::from_center_size(c, Vec2::splat(step));
        let resp = ui.interact(r, Id::new(("tour dot", i)), Sense::click());
        crate::ui::note_control(ui, r, "tour dot");
        let col = if i == page { pal.accent } else if resp.hovered() { pal.text_dim } else { pal.border.lerp_to_gamma(pal.text_dim, 0.35) };
        ui.painter().circle_filled(c, if i == page { 4.0 } else { 3.0 }, col);
        if resp.clicked() {
            *go = Some(i);
        }
    }
    n as f32 * step
}

/// A page's title and its line of text.
fn words(ui: &mut Ui, page: &Page, pal: &Palette) {
    let fs = ui.style().text_styles.get(&egui::TextStyle::Body).map_or(13.0, |f| f.size);
    ui.add(egui::Label::new(egui::RichText::new(page.title).strong().size(fs + 3.0).color(pal.text)).wrap());
    if !page.text.is_empty() {
        ui.add_space(2.0);
        ui.add(egui::Label::new(egui::RichText::new(page.text).color(pal.text_dim)).wrap());
    }
}

/// The commands that reach a page, as key chips with what they do; a command with no
/// keys shows its name, which is how the palette finds it.
fn keys_row(ui: &mut Ui, app: &App, keys: &[Cmd], pal: &Palette) {
    if keys.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        for &c in keys {
            let what = c.info().title.trim_end_matches('…');
            match app.keymap.label(c) {
                Some(k) => {
                    let g = ui.painter().layout_no_wrap(k, FontId::proportional(10.5), pal.text_dim);
                    let (r, _) = ui.allocate_exact_size(g.size() + vec2(10.0, 6.0), Sense::hover());
                    ui.painter().rect(r, CornerRadius::same(4), pal.bg, Stroke::new(1.0, pal.border), StrokeKind::Inside);
                    ui.painter().galley(r.min + vec2(5.0, 3.0), g, pal.text_dim);
                    crate::ui::note_control(ui, r, "key hint");
                    ui.label(egui::RichText::new(what).size(11.0).color(pal.text_dim));
                }
                None => {
                    ui.label(egui::RichText::new(what).size(11.0).color(pal.text));
                }
            }
            ui.add_space(8.0);
        }
    });
}

/// A film, as large as the page leaves room for, its words and keys below. Returns
/// whether it was clicked to play again.
fn film_page(ui: &mut Ui, app: &App, st: &State, body: Rect) -> bool {
    let pal = app.pal;
    let page = &st.pages()[st.page];
    let aspect = st.film.as_ref().map_or(1.6, |f| f.aspect);
    // The words take what they need at this width; the film what is left, up to its
    // size at one pixel for one on a 1.6 screen.
    let fs = ui.style().text_styles.get(&egui::TextStyle::Body).map_or(13.0, |f| f.size);
    let lay = |t: &str, size: f32| ui.painter().layout(t.to_string(), FontId::proportional(size), pal.text, body.width()).size().y;
    let keys_h = 26.0 * (page.keys.len() as f32 * 150.0 / body.width()).ceil();
    let words_h = 10.0 + lay(page.title, fs + 3.0) + 2.0 + lay(page.text, fs) + keys_h + 4.0 + 4.0 * ui.spacing().item_spacing.y;
    let fw = body.width().min(540.0).min(((body.height() - words_h).max(60.0)) * aspect).floor();
    let fr = Rect::from_min_size(pos2((body.center().x - fw / 2.0).round(), body.top()), vec2(fw, (fw / aspect).round()));
    let resp = ui.allocate_rect(fr, Sense::click());
    crate::ui::note_control(ui, fr, "tour film");
    let round = CornerRadius::same(8);
    match st.film.as_ref().and_then(|f| f.tex.as_ref()) {
        Some(t) => {
            egui::Image::new((t.id(), fr.size())).corner_radius(round).paint_at(ui, fr);
        }
        None => {
            ui.painter().rect_filled(fr, round, pal.card());
        }
    }
    ui.painter().rect_stroke(fr, round, Stroke::new(1.0, pal.card_line()), StrokeKind::Outside);
    ui.add_space(10.0);
    words(ui, page, &pal);
    keys_row(ui, app, page.keys, &pal);
    resp.clicked() && st.film_done()
}

/// The first page: a welcome, then the choices in cards under the name of the Options
/// page each is on.
fn setup_page(ui: &mut Ui, app: &mut App, page: &Page, body: Rect) {
    let pal = app.pal;
    words(ui, page, &pal);
    ui.add_space(4.0);
    let rest = Rect::from_min_max(pos2(body.left(), ui.cursor().top()), body.max);
    let mut changed = false;
    let options = app.keymap.label(Cmd::Options).map_or("Options".to_string(), |k| format!("Options ({k})"));
    egui::ScrollArea::vertical().id_salt("tour setup").max_height(rest.height()).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let cw = (ui.available_width() - 10.0).floor();
        ui.add(egui::Label::new(egui::RichText::new(format!("Each stays in {options}, on the page named above it.")).size(11.0).color(pal.text_dim)).wrap());
        for (gi, (section, opts)) in SETUP.iter().enumerate() {
            ui.add_space(if gi == 0 { 6.0 } else { 10.0 });
            let (hr, _) = ui.allocate_exact_size(vec2(cw, 20.0), Sense::hover());
            ui.painter().text(pos2(hr.left() + 4.0, hr.center().y - 1.0), Align2::LEFT_CENTER, *section, FontId::proportional(11.5), pal.text_dim);
            let row_h = 44.0;
            let themes = gi == 0;
            let theme_h = if themes { 58.0 } else { 0.0 };
            let (card, _) = ui.allocate_exact_size(vec2(cw, theme_h + opts.len() as f32 * row_h), Sense::hover());
            ui.painter().rect(card, CornerRadius::same(8), pal.card(), Stroke::new(1.0, pal.card_line()), StrokeKind::Inside);
            if themes {
                changed |= theme_strip(ui, app, Rect::from_min_size(card.min, vec2(cw, theme_h)));
            }
            for (j, &o) in opts.iter().enumerate() {
                let r = Rect::from_min_size(card.min + vec2(0.0, theme_h + j as f32 * row_h), vec2(cw, row_h));
                changed |= pp::option_in(ui, app, o, r, j == 0 && !themes);
            }
        }
        ui.add_space(6.0);
    });
    if changed {
        let ctx = ui.ctx().clone();
        pp::option_changed(app, &ctx);
    }
}

/// A row of themes to pick from, each drawn as a small window in its colours; hovering
/// one shows the whole window in it. Returns whether one was picked.
fn theme_strip(ui: &mut Ui, app: &mut App, r: Rect) -> bool {
    let pal = app.pal;
    let n = THEMES.len() as f32;
    let gap = 8.0;
    let inner = r.shrink2(vec2(12.0, 9.0));
    let cw = ((inner.width() - gap * (n - 1.0)) / n).floor();
    let ch = inner.height();
    let mut picked = false;
    for (i, t) in THEMES.iter().enumerate() {
        let cr = Rect::from_min_size(pos2(inner.left() + i as f32 * (cw + gap), inner.top()), vec2(cw, ch));
        let resp = ui.interact(cr, Id::new(("tour theme", i)), Sense::click()).on_hover_text(t.label());
        crate::ui::note_control(ui, cr, "tour theme");
        pp::theme_card(ui.painter(), cr, crate::theme::palette(*t), 5);
        let on = app.cfg.theme == *t;
        let line = if on { Stroke::new(2.0, pal.accent) } else if resp.hovered() { Stroke::new(1.0, pal.text_dim) } else { Stroke::new(1.0, pal.border) };
        ui.painter().rect_stroke(cr, CornerRadius::same(5), line, StrokeKind::Outside);
        if resp.hovered() {
            app.preview_theme = Some(*t);
        }
        if resp.clicked() && !on {
            app.cfg.theme = *t;
            // A theme picked by hand is the one wanted, whatever the desktop's.
            app.cfg.follow_system_theme = false;
            picked = true;
        }
    }
    let line = Stroke::new(1.0, pal.border.gamma_multiply(0.7));
    ui.painter().line_segment([pos2(r.left() + 12.0, r.bottom() - 0.5), pos2(r.right() - 12.0, r.bottom() - 0.5)], line);
    picked
}

/// Everything else the version added, a card for each group.
fn list_page(ui: &mut Ui, app: &App, body: Rect) {
    let pal = app.pal;
    let fs = ui.style().text_styles.get(&egui::TextStyle::Body).map_or(13.0, |f| f.size);
    ui.add(egui::Label::new(egui::RichText::new(format!("Also new in {NEWS_VERSION}")).strong().size(fs + 3.0).color(pal.text)).wrap());
    ui.add_space(4.0);
    let rest = Rect::from_min_max(pos2(body.left(), ui.cursor().top()), body.max);
    egui::ScrollArea::vertical().id_salt("tour also").max_height(rest.height()).auto_shrink([false, false]).show(ui, |ui| {
        let cw = (ui.available_width() - 10.0).floor();
        ui.set_width(cw);
        for (gi, (group, items)) in ALSO.iter().enumerate() {
            ui.add_space(if gi == 0 { 2.0 } else { 10.0 });
            let (hr, _) = ui.allocate_exact_size(vec2(cw, 20.0), Sense::hover());
            ui.painter().text(pos2(hr.left() + 4.0, hr.center().y - 1.0), Align2::LEFT_CENTER, *group, FontId::proportional(11.5), pal.text_dim);
            pp::card(ui, &pal, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (k, (name, line)) in items.iter().enumerate() {
                    if k > 0 {
                        ui.add_space(6.0);
                    }
                    ui.add(egui::Label::new(egui::RichText::new(*name).size(12.5).color(pal.text)).wrap());
                    ui.add(egui::Label::new(egui::RichText::new(*line).size(11.0).color(pal.text_dim)).wrap());
                }
            });
        }
        ui.add_space(6.0);
    });
}
