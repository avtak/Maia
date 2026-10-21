//! Selecting and copying in the previews. Each view keeps its own selection, between
//! two places of its own kind (a line and a byte, a piece of a document and a character,
//! a cell); this holds what they share: the mouse (a press starts one, Shift extends it,
//! a drag moves its far end, a double-click takes a word and a triple-click a line), the
//! paint, and Ctrl+C and Ctrl+A for the preview that has the keyboard.
//!
//! Only what is on screen is looked at while drawing. Copying a page of a big file reads
//! it on a worker.

use std::cell::RefCell;

use egui::{CornerRadius, Painter, Pos2, Rect, Response, Ui, vec2};

use crate::theme::Palette;

/// A place in line-shaped text: (line, byte in it).
pub type At = (usize, usize);

/// Where Ctrl+A puts the far end: past everything there is.
pub const END: At = (usize::MAX, usize::MAX);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel<P = At> {
    pub anchor: P,
    pub head: P,
}

impl<P: Ord + Copy> Sel<P> {
    pub fn at(p: P) -> Self {
        Sel { anchor: p, head: p }
    }

    pub fn new(anchor: P, head: P) -> Self {
        Sel { anchor, head }
    }

    /// Its two ends in order.
    pub fn span(&self) -> (P, P) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

impl Sel<At> {
    /// The bytes of line `li` (`len` bytes long) inside the selection, and whether the
    /// line break after it is too.
    pub fn on_line(&self, li: usize, len: usize) -> Option<(usize, usize, bool)> {
        let ((l0, b0), (l1, b1)) = self.span();
        if self.is_empty() || li < l0 || li > l1 {
            return None;
        }
        let a = if li == l0 { b0.min(len) } else { 0 };
        let (b, nl) = if li == l1 { (b1.min(len), false) } else { (len, true) };
        (a < b || nl).then_some((a, b, nl))
    }
}

/// Some text: the selection is not empty.
pub fn some<P: Ord + Copy>(s: &Option<Sel<P>>) -> bool {
    s.as_ref().is_some_and(|s| !s.is_empty())
}

/// The selected text of lines `0..n`, one line at a time from `line`.
pub fn lines_text<'a>(sel: &Sel, n: usize, line: impl Fn(usize) -> Option<std::borrow::Cow<'a, str>>) -> String {
    let ((l0, _), (l1, _)) = sel.span();
    let mut out = String::new();
    for li in l0..=l1.min(n.saturating_sub(1)) {
        let Some(l) = line(li) else { continue };
        if let Some((a, b, nl)) = sel.on_line(li, l.len()) {
            out.push_str(l.get(a..b).unwrap_or(""));
            // No line break after the last line there is.
            if nl && li + 1 < n {
                out.push('\n');
            }
        }
    }
    out
}

/// What kind of character `c` is, for a double-click: part of a word, a space, or
/// anything else (each of those on its own).
fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        1
    } else if c.is_whitespace() {
        2
    } else {
        0
    }
}

/// The word around byte `b` of `s`: the run of word characters (or spaces) it is in,
/// else the one character there.
pub fn word(s: &str, b: usize) -> (usize, usize) {
    let b = s.floor_char_boundary(b.min(s.len()));
    // At the end of a line, the word before it.
    let (at, c) = match s[b..].chars().next() {
        Some(c) => (b, c),
        None => match s[..b].char_indices().next_back() {
            Some(x) => x,
            None => return (0, 0),
        },
    };
    let k = class(c);
    if k == 0 {
        return (at, at + c.len_utf8());
    }
    let start = s[..at].char_indices().rev().take_while(|(_, x)| class(*x) == k).last().map_or(at, |(i, _)| i);
    let end = s[at..].char_indices().find(|(_, x)| class(*x) != k).map_or(s.len(), |(i, _)| at + i);
    (start, end)
}

/// The byte of `s` nearest a column, counting wide characters as two.
pub fn byte_at_col(s: &str, col: f32) -> usize {
    let mut x = 0.0;
    for (i, c) in s.char_indices() {
        let w = crate::inspector::char_cols(c) as f32;
        if x + w * 0.5 > col {
            return i;
        }
        x += w;
    }
    s.len()
}

/// Columns the bytes `a..b` of `s` take.
pub fn cols(s: &str, a: usize, b: usize) -> f32 {
    s.get(a..b).map_or(0, |t| t.chars().map(crate::inspector::char_cols).sum::<usize>()) as f32
}

/// The mouse over a selectable view whose response is `resp`. `hit` turns a point into a
/// place; `word` and `line` give what a double and a triple click take around one.
/// Returns true when the selection changed.
pub fn track<P: Ord + Copy>(ui: &Ui, resp: &Response, sel: &mut Option<Sel<P>>, hit: impl Fn(Pos2) -> P, word: impl Fn(P) -> (P, P), line: impl Fn(P) -> (P, P)) -> bool {
    let (pressed, down, shift, origin, pos) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.modifiers.shift, i.pointer.press_origin(), i.pointer.interact_pos()));
    let before = *sel;
    let on = resp.is_pointer_button_down_on();
    if pressed && on {
        if let Some(p) = origin.map(&hit) {
            match sel.as_mut() {
                Some(s) if shift => s.head = p,
                _ => *sel = Some(Sel::at(p)),
            }
        }
    } else if on && down {
        if let (Some(s), Some(pp)) = (sel.as_mut(), pos) {
            s.head = hit(pp);
        }
        if let Some(pp) = pos {
            autoscroll(ui, pp);
        }
    }
    if let Some(p) = pos.filter(|_| resp.double_clicked() || resp.triple_clicked()).map(&hit) {
        let (a, b) = if resp.triple_clicked() { line(p) } else { word(p) };
        *sel = Some(Sel::new(a, b));
    }
    *sel != before
}

/// A drag held past the edge of what shows: the view scrolls on towards it.
pub fn autoscroll(ui: &Ui, pp: Pos2) {
    let clip = ui.clip_rect();
    let d = vec2(edge_push(pp.x, clip.left(), clip.right()), edge_push(pp.y, clip.top(), clip.bottom()));
    if d != egui::Vec2::ZERO {
        ui.scroll_with_delta(d);
        ui.ctx().request_repaint();
    }
}

/// How far to scroll for a pointer at `v`, held past `lo` or `hi`.
fn edge_push(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        ((lo - v) * 0.5).clamp(2.0, 40.0)
    } else if v > hi {
        -((v - hi) * 0.5).clamp(2.0, 40.0)
    } else {
        0.0
    }
}

thread_local! {
    /// Where selections were painted this frame, for a right-click on one.
    static PAINTED: RefCell<Vec<Rect>> = const { RefCell::new(Vec::new()) };
}

/// Paint part of a selection, behind its text, as the editor paints its own.
pub fn paint(p: &Painter, r: Rect, pal: &Palette) {
    p.rect_filled(r, CornerRadius::ZERO, pal.selected);
    let vis = r.intersect(p.clip_rect());
    if vis.is_positive() {
        PAINTED.with(|v| v.borrow_mut().push(vis));
    }
}

/// Start over for a preview about to be drawn.
pub fn forget_painted() {
    PAINTED.with(|v| v.borrow_mut().clear());
}

/// Whether `p` is on a selection painted since `forget_painted`.
pub fn on_painted(p: Pos2) -> bool {
    PAINTED.with(|v| v.borrow().iter().any(|r| r.expand(1.0).contains(p)))
}

/// What was copied, said in a few words.
pub fn said(s: &str) -> String {
    let lines = s.lines().count();
    let chars = s.chars().count();
    if lines <= 1 && chars <= 40 {
        format!("Copied “{}”", s.trim_end_matches('\n'))
    } else if lines > 1 {
        format!("Copied {} lines", crate::table::fmt_count(lines))
    } else {
        format!("Copied {} characters", crate::table::fmt_count(chars))
    }
}

/// Ctrl+C (`copy`) or Ctrl+A for the active panel's preview, when it has the keyboard
/// and something to take them. False leaves the key to the list: with nothing selected
/// in the preview, Ctrl+C copies the files as it always does.
pub fn key(app: &mut crate::app::App, ctx: &egui::Context, copy: bool) -> bool {
    let id = app.active;
    let Some(ins) = app.panels.get_mut(&id).and_then(|p| p.inspector.as_mut()).filter(|i| i.keys) else { return false };
    if !copy {
        return ins.select_all();
    }
    match ins.copy_selection(ctx) {
        Copied::Now(s) => {
            ctx.copy_text(s.clone());
            app.toast(said(&s));
            true
        }
        Copied::Later => true,
        Copied::Nothing => false,
    }
}

/// The right-click menu's Copy and Select All, for panel `id`'s preview.
pub fn menu_act(app: &mut crate::app::App, id: u64, copy: bool) {
    let ctx = app.ctx.clone();
    let Some(ins) = app.panels.get_mut(&id).and_then(|p| p.inspector.as_mut()) else { return };
    ins.keys = true;
    if !copy {
        ins.select_all();
        return;
    }
    if let Copied::Now(s) = ins.copy_selection(&ctx) {
        ctx.copy_text(s.clone());
        app.toast(said(&s));
    }
}

pub enum Copied {
    /// The text, to go on the clipboard now.
    Now(String),
    /// Being read on a worker; it lands on the clipboard when read.
    Later,
    Nothing,
}

/// Most a copy out of a big file takes (the clipboard holds it all in memory, twice).
pub const MAX_COPY: u64 = 32 << 20;
