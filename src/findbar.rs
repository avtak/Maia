//! Find in a preview: the bar along its bottom and the matches it marks in the text.

use egui::{CornerRadius, FontId, Rect, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2};

use crate::theme::{self, Glyph, Palette, paint_glyph};

pub const BAR_H: f32 = 30.0;
/// Matches marked; past it the count says "+".
const MAX: usize = 50_000;

/// A match: its line, and where in the line it starts and ends (bytes).
pub type Hit = (u32, u32, u32);

pub struct Finder {
    pub query: String,
    /// The replacement, while the editor's Replace row is open.
    pub replace: Option<String>,
    /// Put the keyboard in the field (and select what is in it) on the next frame.
    pub focus: bool,
    pub hits: Vec<Hit>,
    /// The query and the text the hits were found for.
    key: (String, u64, usize),
    pub cur: usize,
    /// Scroll the current match into view on the next frame.
    pub reveal: bool,
    /// The field had the keyboard last frame (egui lets go of it on Escape before the
    /// app sees the key, so it is remembered here).
    pub focused: bool,
    opened: Option<f64>,
}

#[derive(PartialEq, Eq)]
pub enum Out {
    None,
    Next,
    Prev,
    Close,
    Replace,
    ReplaceAll,
}

impl Finder {
    pub fn new(query: String) -> Finder {
        Finder { query, replace: None, focus: true, hits: vec![], key: (String::new(), u64::MAX, 0), cur: 0, reveal: false, focused: false, opened: None }
    }

    /// Upper case in the query makes it match case.
    pub fn case(&self) -> bool {
        self.query.chars().any(char::is_uppercase)
    }

    /// Find the query in `lines` again when it or the text (`stamp`, the line count)
    /// changed; the match shown stays the one at or after the line it was on.
    pub fn update<S: AsRef<str>>(&mut self, lines: &[S], stamp: u64) {
        let key = (self.query.clone(), stamp, lines.len());
        if self.key == key {
            return;
        }
        let near = self.hits.get(self.cur).map_or(0, |h| h.0);
        let fresh = self.key.0 != key.0;
        self.key = key;
        self.hits.clear();
        if self.query.is_empty() {
            return;
        }
        let case = self.case();
        let mut at = vec![];
        for (i, l) in lines.iter().enumerate() {
            at.clear();
            find_in(l.as_ref(), &self.query, case, &mut at);
            self.hits.extend(at.iter().map(|&(a, b)| (i as u32, a, b)));
            if self.hits.len() >= MAX {
                break;
            }
        }
        self.cur = self.hits.iter().position(|h| h.0 >= near).unwrap_or(0);
        // A new query shows its first match; text that grew keeps the place.
        self.reveal |= fresh && !self.hits.is_empty();
    }

    pub fn step(&mut self, d: isize) {
        let n = self.hits.len();
        if n > 0 {
            self.cur = (self.cur as isize + d).rem_euclid(n as isize) as usize;
            self.reveal = true;
        }
    }

    /// "3 of 12", or why there are none.
    pub fn count(&self) -> String {
        match self.hits.len() {
            _ if self.query.is_empty() => String::new(),
            0 => "No matches".into(),
            n => format!("{} of {}{}", self.cur + 1, crate::table::fmt_count(n), if n >= MAX { "+" } else { "" }),
        }
    }
}

/// Where `q` occurs in `line`: ASCII letters ignoring case unless `case`.
pub fn find_in(line: &str, q: &str, case: bool, out: &mut Vec<(u32, u32)>) {
    let (h, n) = (line.as_bytes(), q.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return;
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        let w = &h[i..i + n.len()];
        if if case { w == n } else { w.eq_ignore_ascii_case(n) } {
            out.push((i as u32, (i + n.len()) as u32));
            i += n.len();
        } else {
            i += 1;
        }
    }
}

/// The bar, sliding up from the bottom of `body` as it opens; `any`: there are matches to
/// step through. Returns what was asked and the room left above it.
#[allow(clippy::too_many_arguments)]
pub fn bar(ui: &mut Ui, body: Rect, f: &mut Finder, count: &str, any: bool, id: u64, animations: bool, pal: &Palette) -> (Out, Rect) {
    let now = ui.input(|i| i.time);
    let t = if animations { ((now - *f.opened.get_or_insert(now)) / 0.12).min(1.0) as f32 } else { 1.0 };
    if t < 1.0 {
        ui.ctx().request_repaint();
    }
    let ease = 1.0 - (1.0 - t) * (1.0 - t);
    let rows = if f.replace.is_some() { 2.0 } else { 1.0 };
    // A body shorter than the bar (a terminal pane closing or squeezed by a small
    // window) shows none of it, rather than a bar reaching up over what is above.
    if body.height() < BAR_H * rows {
        return (Out::None, body);
    }
    let h = (BAR_H * rows * ease).round();
    let bar = Rect::from_min_max(pos2(body.left(), body.bottom() - h), body.max);
    let mut out = Out::None;
    let p = ui.painter().with_clip_rect(bar.intersect(ui.clip_rect()));
    p.rect_filled(bar, CornerRadius::ZERO, pal.bg);
    p.line_segment([pos2(bar.left(), bar.top() - 0.5), pos2(bar.right(), bar.top() - 0.5)], Stroke::new(1.0, pal.border));
    let top = Rect::from_min_size(bar.min, vec2(bar.width(), BAR_H));
    let mut bui = ui.new_child(UiBuilder::new().max_rect(top.shrink2(vec2(6.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    bui.set_clip_rect(bar.intersect(ui.clip_rect()));
    bui.set_opacity(ease);
    bui.spacing_mut().item_spacing.x = 2.0;
    let (gr, _) = bui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
    paint_glyph(bui.painter(), Rect::from_center_size(gr.center(), Vec2::splat(13.0)), Glyph::Search, pal.text_dim);
    let none = !f.query.is_empty() && !any && count == "No matches";
    let cg = bui.painter().layout_no_wrap(count.to_string(), FontId::proportional(11.5), if none { pal.danger } else { pal.text_dim });
    // Right to left: the buttons and the count keep their room, the field takes the rest.
    let fid = egui::Id::new(("insp-find", id));
    let mut r = None;
    bui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if crate::ui::glyph_button(ui, Glyph::Close, "Close (Esc)", true, pal).clicked() {
            out = Out::Close;
        }
        for (icon, tip, o) in [(Glyph::ChevronDown, "Next match (Enter)", Out::Next), (Glyph::ChevronUp, "Previous match (Shift+Enter)", Out::Prev)] {
            if icon_button(ui, icon, tip, any, pal) {
                out = o;
            }
        }
        // In a narrow panel the field keeps room to type in; the count gives way.
        if ui.available_width() - cg.size().x - 10.0 >= 40.0 {
            let (cr, _) = ui.allocate_exact_size(cg.size() + vec2(10.0, 0.0), Sense::hover());
            ui.painter().galley(pos2(cr.left() + 4.0, cr.center().y - cg.size().y / 2.0), cg, pal.text_dim);
        }
        let te = egui::TextEdit::singleline(&mut f.query).id(fid).hint_text(egui::RichText::new("Find").color(pal.text_dim)).frame(false).desired_width(ui.available_width()).font(FontId::proportional(12.5));
        r = Some(ui.add(te));
    });
    let Some(r) = r else { return (out, body) };
    crate::ui::note_control(&bui, r.rect, "find field");
    if f.focus {
        f.focus = false;
        r.request_focus();
        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), fid) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(f.query.chars().count()));
            st.cursor.set_char_range(Some(all));
            st.store(ui.ctx(), fid);
        }
    }
    if f.focused {
        bui.input_mut(|i| {
            if i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter) || i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3) {
                out = Out::Prev;
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) || i.consume_key(egui::Modifiers::NONE, egui::Key::F3) {
                out = Out::Next;
            }
        });
        // Enter takes the focus away from a single-line field: kept for the next press.
        if out == Out::Next || out == Out::Prev {
            r.request_focus();
        }
    }
    f.focused = ui.memory(|m| m.has_focus(fid));
    if let Some(rep) = f.replace.as_mut() {
        let row = Rect::from_min_size(pos2(bar.left(), top.bottom()), vec2(bar.width(), BAR_H));
        let mut rui = ui.new_child(UiBuilder::new().max_rect(row.shrink2(vec2(6.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        rui.set_clip_rect(bar.intersect(ui.clip_rect()));
        rui.set_opacity(ease);
        rui.spacing_mut().item_spacing.x = 6.0;
        let (gr, _) = rui.allocate_exact_size(vec2(16.0, 20.0), Sense::hover());
        theme::paint_lucide(rui.painter(), Rect::from_center_size(gr.center(), Vec2::splat(13.0)), "replace", pal.text_dim);
        let w = (rui.available_width() - 150.0).max(40.0);
        let r = rui.add(egui::TextEdit::singleline(rep).id_salt(("insp-replace", id)).hint_text(egui::RichText::new("Replace").color(pal.text_dim)).frame(false).desired_width(w).font(FontId::proportional(12.5)));
        crate::ui::note_control(&rui, r.rect, "replace field");
        if r.lost_focus() && rui.input(|i| i.key_pressed(egui::Key::Enter)) {
            out = Out::Replace;
            r.request_focus();
        }
        let on = any;
        if small_button(&mut rui, "Replace", on, pal) {
            out = Out::Replace;
        }
        if small_button(&mut rui, "All", on, pal) {
            out = Out::ReplaceAll;
        }
    }
    (out, Rect::from_min_max(body.min, pos2(body.right(), bar.top() - 1.0)))
}

fn icon_button(ui: &mut Ui, icon: Glyph, tip: &'static str, enabled: bool, pal: &Palette) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), if enabled { Sense::click() } else { Sense::hover() });
    crate::ui::note_control(ui, r, tip);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(4), pal.hover);
    }
    let col = if enabled { pal.text_dim } else { pal.text_dim.gamma_multiply(0.4) };
    paint_glyph(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(12.0)), icon, col);
    enabled && resp.on_hover_text(tip).clicked()
}

fn small_button(ui: &mut Ui, label: &'static str, enabled: bool, pal: &Palette) -> bool {
    let b = egui::Button::new(egui::RichText::new(label).size(11.5)).fill(pal.card()).stroke(Stroke::new(1.0, pal.card_line())).corner_radius(4.0).min_size(vec2(0.0, 20.0));
    let r = ui.add_enabled(enabled, b);
    crate::ui::note_control(ui, r.rect, label);
    r.clicked()
}

/// Paint the matches on one row of a text preview: `cols(a, b)` gives the columns of the
/// line's bytes a..b; `x0` is where the row's text starts and `cw` a column's width.
#[allow(clippy::too_many_arguments)]
pub fn paint_hits(p: &egui::Painter, hits: &[Hit], cur: Option<usize>, line: u32, a: usize, b: usize, text: &str, x0: f32, top: f32, h: f32, cw: f32, pal: &Palette) {
    let first = hits.partition_point(|x| x.0 < line);
    for (k, &(_, s, e)) in hits[first..].iter().take_while(|x| x.0 == line).enumerate() {
        let (s, e) = ((s as usize).max(a), (e as usize).min(b));
        if s >= e {
            continue;
        }
        let cols = |x: usize, y: usize| text.get(x..y).map_or(0, |t| t.chars().map(crate::inspector::char_cols).sum::<usize>()) as f32;
        let r = Rect::from_min_size(pos2(x0 + cols(a, s) * cw, top + 1.0), vec2((cols(s, e) * cw).max(2.0), h - 2.0));
        if cur == Some(first + k) {
            p.rect_filled(r.expand(1.0), CornerRadius::same(3), pal.accent.gamma_multiply(0.55));
            p.rect_stroke(r.expand(1.0), CornerRadius::same(3), Stroke::new(1.0, pal.accent), egui::StrokeKind::Outside);
        } else {
            p.rect_filled(r, CornerRadius::same(2), pal.accent.gamma_multiply(0.25));
        }
    }
}

/// The x where a match starts, for scrolling sideways to it.
pub fn hit_x(text: &str, start: usize, cw: f32) -> f32 {
    text.get(..start).map_or(0, |t| t.chars().map(crate::inspector::char_cols).sum::<usize>()) as f32 * cw
}
