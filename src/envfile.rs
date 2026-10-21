//! `.env` files: keys stay readable, values are hidden until asked for, a line at a
//! time (a click) or all at once (the toolbar's lock). The mask has one length, so it
//! says nothing about the secret behind it.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, CornerRadius, FontId, Rect, Sense, Ui, UiBuilder, pos2, vec2};

use crate::theme::Palette;

pub const MASK: &str = "••••••••";

pub fn is_env_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l == ".env" || l.starts_with(".env.") || (l.ends_with(".env") && l.len() > 4)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Blank,
    Comment,
    /// `KEY=value`: where the value starts (after `=` and spaces).
    Pair { key_end: usize, value: usize },
    /// Inside a quoted value that spans lines, or not a `KEY=value` line at all: hidden
    /// whole.
    Hidden,
}

/// Each line's kind, following quoted values over line breaks.
pub fn classify(lines: &[String]) -> Vec<Line> {
    let mut out = Vec::with_capacity(lines.len());
    let mut open: Option<char> = None;
    for l in lines {
        if let Some(q) = open {
            if l.contains(q) {
                open = None;
            }
            out.push(Line::Hidden);
            continue;
        }
        let t = l.trim_start();
        if t.is_empty() {
            out.push(Line::Blank);
            continue;
        }
        if t.starts_with('#') {
            out.push(Line::Comment);
            continue;
        }
        let lead = l.len() - t.len();
        let body = t.strip_prefix("export ").map_or(t, |r| r.trim_start());
        let skip = lead + (t.len() - body.len());
        let key_len = body.bytes().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')).count();
        let after = &body[key_len..];
        let eq = after.trim_start();
        if key_len == 0 || !eq.starts_with(['=', ':']) {
            out.push(Line::Hidden);
            continue;
        }
        let v = eq[1..].trim_start();
        let value = l.len() - v.len();
        if let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'' || *c == '`') {
            if !v[1..].contains(q) {
                open = Some(q);
            }
        }
        out.push(Line::Pair { key_end: skip + key_len, value });
    }
    out
}

/// A line as shown: the key, then its value or the mask.
pub fn shown(line: &str, kind: Line, reveal: bool) -> String {
    match kind {
        Line::Pair { value, .. } if !reveal && value < line.len() => format!("{}{MASK}", &line[..value]),
        Line::Hidden if !reveal => MASK.into(),
        _ => line.to_string(),
    }
}

pub struct View {
    pub kinds: Vec<Line>,
    /// Lines shown as they are.
    pub open: std::collections::HashSet<usize>,
}

impl View {
    pub fn new(lines: &[String]) -> View {
        View { kinds: classify(lines), open: Default::default() }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn view(ui: &mut Ui, body: Rect, lines: &[String], v: &mut View, all: bool, numbers: bool, size: f32, pal: &Palette, spacing: f32, sel: &mut Option<crate::textsel::Sel>) {
    let font = FontId::monospace(size);
    let row_h = crate::inspector::row_height(ui, size, spacing);
    let (line_h, cw) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, 'M')));
    let lift = ((row_h - line_h) / 2.0).floor().max(0.0);
    let gutter = if numbers { (lines.len().max(1).to_string().len() as f32) * size * 0.62 + 16.0 } else { 6.0 };
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(4.0, 4.0))));
    let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * cw + gutter + 20.0;
    egui::ScrollArea::both().auto_shrink([false, false]).id_salt("insp_env").show_viewport(&mut inner, |ui, vp| {
        let (r, resp) = ui.allocate_exact_size(vec2(widest.max(vp.width()), lines.len() as f32 * row_h + 8.0), Sense::click_and_drag());
        // Selected as shown: a hidden value is its mask until it is revealed.
        let text = |i: usize| lines.get(i).map_or(String::new(), |l| shown(l, v.kinds.get(i).copied().unwrap_or(Line::Hidden), all || v.open.contains(&i)));
        if !lines.is_empty() {
            let hit = |pp: egui::Pos2| -> crate::textsel::At {
                let y = pp.y - r.top();
                if y < 0.0 {
                    return (0, 0);
                }
                let i = (y / row_h) as usize;
                if i >= lines.len() {
                    return (lines.len() - 1, text(lines.len() - 1).len());
                }
                (i, crate::textsel::byte_at_col(&text(i), (pp.x - r.left() - gutter) / cw.max(1.0)))
            };
            let word = |(i, b): crate::textsel::At| {
                let (x, y) = crate::textsel::word(&text(i), b);
                ((i, x), (i, y))
            };
            let line = |(i, _): crate::textsel::At| if i + 1 < lines.len() { ((i, 0), (i + 1, 0)) } else { ((i, 0), (i, text(i).len())) };
            crate::textsel::track(ui, &resp, sel, hit, word, line);
        }
        let first = (vp.min.y / row_h).floor().max(0.0) as usize;
        let last = ((vp.max.y / row_h).ceil() as usize + 1).min(lines.len());
        let p = ui.painter();
        let hover = resp.hover_pos();
        let fmt = |c| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
        let mut hovered_secret = None;
        for i in first..last {
            let top = r.top() + i as f32 * row_h;
            let y = top + lift;
            let (line, kind) = (&lines[i], v.kinds.get(i).copied().unwrap_or(Line::Hidden));
            let reveal = all || v.open.contains(&i);
            let secret = matches!(kind, Line::Pair { .. } | Line::Hidden);
            let row = Rect::from_min_max(pos2(r.left(), top), pos2(r.right(), top + row_h));
            if secret && hover.is_some_and(|h| row.contains(h)) {
                p.rect_filled(row, CornerRadius::same(3), pal.hover);
                hovered_secret = Some(i);
            }
            if numbers {
                p.text(pos2(r.left() + gutter - 10.0, y), Align2::RIGHT_TOP, (i + 1).to_string(), font.clone(), pal.text_dim.gamma_multiply(0.7));
            }
            if let Some((sa, sb, nl)) = sel.filter(|s| !s.is_empty()).and_then(|s| s.on_line(i, usize::MAX)) {
                let t = shown(line, kind, reveal);
                let (sa, sb) = (sa.min(t.len()), sb.min(t.len()));
                let x0 = r.left() + gutter + crate::textsel::cols(&t, 0, sa) * cw;
                let x1 = r.left() + gutter + crate::textsel::cols(&t, 0, sb) * cw + if nl { cw * 0.6 } else { 0.0 };
                if x1 > x0 {
                    crate::textsel::paint(p, Rect::from_min_max(pos2(x0, top), pos2(x1, top + row_h)), pal);
                }
            }
            let mut job = LayoutJob::default();
            match kind {
                Line::Comment => job.append(line, 0.0, fmt(pal.syntax.comment)),
                Line::Blank => {}
                Line::Pair { key_end, value } => {
                    job.append(&line[..key_end], 0.0, fmt(pal.syntax.keyword));
                    job.append(&line[key_end..value], 0.0, fmt(pal.text_dim));
                    if reveal {
                        job.append(&line[value..], 0.0, fmt(pal.syntax.string));
                    } else if value < line.len() {
                        job.append(MASK, 0.0, fmt(pal.text_dim));
                    }
                }
                Line::Hidden => job.append(&shown(line, kind, reveal), 0.0, fmt(if reveal { pal.syntax.string } else { pal.text_dim })),
            }
            if !job.text.is_empty() {
                let g = ui.fonts_mut(|f| f.layout_job(job));
                p.galley(pos2(r.left() + gutter, y), g, pal.text);
            }
        }
        if let Some(i) = hovered_secret {
            if resp.clicked() && !all {
                if !v.open.remove(&i) {
                    v.open.insert(i);
                }
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            resp.on_hover_text(if all || v.open.contains(&i) { "Hide this value" } else { "Show this value" });
        }
    });
}
