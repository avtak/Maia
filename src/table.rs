//! Delimited text (CSV, TSV and their cousins) read into rows, and the table the
//! inspector draws for it and for SQLite results: a header that stays put, row numbers
//! that stay put, columns sized to what they hold, scrolling both ways.

use egui::{Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use crate::theme::Palette;

/// Rows kept for a preview; a bigger file shows these with a note.
pub const MAX_ROWS: usize = 100_000;
/// Characters kept of one cell (a whole document in one quoted field stays readable).
const MAX_CELL: usize = 2000;

pub struct Table {
    /// Column titles: the file's first row, or None when that row is data.
    pub header: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
    pub cols: usize,
    /// Columns whose cells are all numbers (right-aligned).
    pub numeric: Vec<bool>,
    /// Why not everything is shown, when it is not.
    pub note: Option<String>,
}

/// The delimiter a file uses: tabs for `.tsv`, else whichever of comma, tab, semicolon
/// and bar splits the first records into the same number of fields most often.
pub fn detect(sample: &str, name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tsv") || lower.ends_with(".tab") {
        return b'\t';
    }
    let mut best = (b',', 0usize);
    for d in [b',', b'\t', b';', b'|'] {
        let (rows, _) = parse(sample, d, 40);
        let counts: Vec<usize> = rows.iter().map(|r| r.len()).filter(|&n| n > 0).collect();
        if counts.is_empty() {
            continue;
        }
        // The most common field count, weighted by how many records have it: a stray
        // comma inside a semicolon file does not win.
        let mut modes: Vec<(usize, usize)> = vec![];
        for &c in &counts {
            match modes.iter_mut().find(|m| m.0 == c) {
                Some(m) => m.1 += 1,
                None => modes.push((c, 1)),
            }
        }
        let (fields, n) = modes.iter().copied().max_by_key(|m| (m.1, m.0)).unwrap_or((1, 0));
        let score = if fields > 1 { n * 64 + fields } else { 0 };
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

/// Records of `text` split at `delim`, as RFC 4180 has it: a field in double quotes may
/// hold the delimiter, line breaks and doubled quotes. Stops after `max` records; the
/// flag says whether there was more.
pub fn parse(text: &str, delim: u8, max: usize) -> (Vec<Vec<String>>, bool) {
    let b = text.as_bytes();
    let mut rows: Vec<Vec<String>> = vec![];
    let mut row: Vec<String> = vec![];
    let mut field: Vec<u8> = vec![];
    let (mut i, mut quoted, mut at_start) = (0, false, true);
    let push_field = |field: &mut Vec<u8>, row: &mut Vec<String>| {
        let mut s = String::from_utf8_lossy(field).into_owned();
        if s.len() > MAX_CELL {
            s.truncate(s.floor_char_boundary(MAX_CELL));
            s.push('…');
        }
        row.push(s);
        field.clear();
    };
    while i < b.len() {
        let c = b[i];
        if quoted {
            if c == b'"' {
                if b.get(i + 1) == Some(&b'"') {
                    field.push(b'"');
                    i += 2;
                    continue;
                }
                quoted = false;
            } else {
                field.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            b'"' if at_start => {
                quoted = true;
                at_start = false;
            }
            b'\r' if b.get(i + 1) == Some(&b'\n') => {}
            b'\n' | b'\r' => {
                push_field(&mut field, &mut row);
                // A blank line is no record.
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(std::mem::take(&mut row));
                    if rows.len() >= max {
                        return (rows, i + 1 < b.len());
                    }
                }
                row.clear();
                at_start = true;
            }
            _ if c == delim => {
                push_field(&mut field, &mut row);
                at_start = true;
            }
            _ => {
                field.push(c);
                at_start = false;
            }
        }
        i += 1;
    }
    if !field.is_empty() || !row.is_empty() || quoted {
        push_field(&mut field, &mut row);
        rows.push(row);
    }
    (rows, false)
}

/// A number as tables write them: a sign, digits grouped by commas or underscores, one
/// decimal point, an exponent, a percent sign. A date is not one.
pub fn is_number(s: &str) -> bool {
    let t = s.trim();
    let t = t.strip_prefix(['-', '+']).unwrap_or(t);
    let t = t.strip_suffix('%').unwrap_or(t);
    let (m, e) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let mantissa = m.bytes().any(|c| c.is_ascii_digit()) && m.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b',' | b'_')) && m.bytes().filter(|&c| c == b'.').count() <= 1;
    mantissa && e.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

impl Table {
    /// Rows as read; the first is the header unless it looks like data (a number in it).
    pub fn new(mut rows: Vec<Vec<String>>, more: bool, header: Option<bool>) -> Table {
        let has_header = header.unwrap_or_else(|| rows.len() > 1 && rows[0].iter().all(|c| !is_number(c)));
        let header = (has_header && !rows.is_empty()).then(|| rows.remove(0));
        let cols = rows.iter().map(|r| r.len()).chain(header.iter().map(|h| h.len())).max().unwrap_or(0);
        let numeric = (0..cols).map(|c| {
            let mut seen = false;
            for r in rows.iter().take(2000) {
                match r.get(c).map(|s| s.trim()) {
                    Some("") | None => {}
                    Some(s) if is_number(s) => seen = true,
                    Some(_) => return false,
                }
            }
            seen
        }).collect();
        let note = more.then(|| format!("First {} rows", fmt_count(rows.len())));
        Table { header, rows, cols, numeric, note }
    }

    /// A delimited text file, from its first bytes (`cut`: the file goes on past them).
    pub fn from_text(text: &str, name: &str, cut: bool) -> Table {
        let d = detect(&text[..text.floor_char_boundary(64 << 10)], name);
        let (rows, more) = parse(text, d, MAX_ROWS + 1);
        let mut t = Table::new(rows, more || cut, None);
        if more || cut {
            t.rows.truncate(MAX_ROWS);
            t.note = Some(format!("First {} rows", fmt_count(t.rows.len())));
        }
        t
    }
}

pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The table and what drawing it needs to remember: column widths for a font size, and
/// the cells selected.
pub struct TableView {
    pub t: Table,
    widths: Option<(f32, Vec<f32>)>,
    /// A rectangle of cells, between two corners (row, column).
    pub sel: Option<crate::textsel::Sel<(usize, usize)>>,
    grab: Grab,
}

/// What the selection was taken by: cells, whole rows (their numbers), whole columns
/// (their titles), or all of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grab {
    Cells,
    Rows,
    Cols,
    All,
}

impl TableView {
    pub fn new(t: Table) -> TableView {
        TableView { t, widths: None, sel: None, grab: Grab::Cells }
    }

    pub fn select_all(&mut self) -> bool {
        if self.t.rows.is_empty() || self.t.cols == 0 {
            return false;
        }
        self.sel = Some(crate::textsel::Sel::new((0, 0), (self.t.rows.len() - 1, self.t.cols - 1)));
        self.grab = Grab::All;
        true
    }

    /// The selected cells' (first row, first column) and (last row, last column).
    pub fn sel_rect(&self) -> Option<((usize, usize), (usize, usize))> {
        corners(self.sel)
    }

    /// The cells selected as tab-separated lines, as spreadsheets paste them: a field
    /// holding a tab, a line break or a quote in quotes. Whole columns bring their titles.
    pub fn copy_text(&self) -> String {
        let Some(((r0, c0), (r1, c1))) = self.sel_rect() else { return String::new() };
        let field = |s: &str| if s.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
        let line = |cells: &[String]| (c0..=c1).map(|c| cells.get(c).map_or(String::new(), |s| field(s))).collect::<Vec<_>>().join("\t");
        let mut out = vec![];
        if matches!(self.grab, Grab::Cols | Grab::All) {
            if let Some(h) = &self.t.header {
                out.push(line(h));
            }
        }
        for r in r0..=r1.min(self.t.rows.len().saturating_sub(1)) {
            out.push(line(&self.t.rows[r]));
        }
        out.join("\n")
    }
}

fn corners(s: Option<crate::textsel::Sel<(usize, usize)>>) -> Option<((usize, usize), (usize, usize))> {
    let s = s?;
    let ((a, b), (c, d)) = (s.anchor, s.head);
    Some(((a.min(c), b.min(d)), (a.max(c), b.max(d))))
}

/// A cell's text on one line.
fn one_line(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\n', '\r', '\t']) { s.replace("\r\n", " ↵ ").replace(['\n', '\r'], " ↵ ").replace('\t', " ").into() } else { s.into() }
}

/// Column title when the file has none: A, B, … Z, AA, AB …
fn letters(mut i: usize) -> String {
    let mut s = vec![];
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

pub fn view(ui: &mut Ui, body: Rect, v: &mut TableView, size: f32, pal: &Palette, spacing: f32, id: impl std::hash::Hash) {
    let t = &v.t;
    if t.cols == 0 {
        ui.painter().text(body.center(), Align2::CENTER_CENTER, "Empty table", FontId::proportional(12.5), pal.text_dim);
        return;
    }
    let font = FontId::proportional(size);
    let row_h = crate::inspector::row_height(ui, size, spacing);
    let pad = 8.0;
    // Widths fitted to the header and the first rows, between 3 and 40 characters.
    if v.widths.as_ref().is_none_or(|w| w.0 != size) {
        let em = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let widths = ui.fonts_mut(|f| {
            let mut w = vec![em * 3.0; t.cols];
            let mut measure = |c: usize, s: &str| {
                let mut x = 0.0;
                for ch in one_line(s).chars().take(48) {
                    x += f.glyph_width(&font, ch);
                }
                w[c] = w[c].max(x.min(em * 40.0));
            };
            if let Some(h) = &t.header {
                for (c, s) in h.iter().enumerate() {
                    measure(c, s);
                }
            }
            for r in t.rows.iter().take(400) {
                for (c, s) in r.iter().enumerate() {
                    measure(c, s);
                }
            }
            w.iter().map(|x| (x + pad * 2.0).ceil()).collect::<Vec<f32>>()
        });
        v.widths = Some((size, widths));
    }
    let Some((_, widths)) = &v.widths else { return };
    let t = &v.t;
    let gutter = (fmt_count(t.rows.len().max(1)).len() as f32 * size * 0.6 + 18.0).round();
    let total_w: f32 = gutter + widths.iter().sum::<f32>();
    let n = t.rows.len() + usize::from(t.note.is_some());
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(2.0, 2.0))));
    let lift = ((row_h - ui.fonts_mut(|f| f.row_height(&font))) / 2.0).floor().max(0.0);
    let (mut sel, mut grab) = (v.sel, v.grab);
    egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("table", id)).show_viewport(&mut inner, |ui, vp| {
        let (r, resp) = ui.allocate_exact_size(vec2(total_w.max(vp.width()), (n + 1) as f32 * row_h + 4.0), Sense::click_and_drag());
        let clip = ui.clip_rect();
        let p = ui.painter().clone();
        // Rows below the sticky header, cells right of the sticky row numbers.
        let head_y = r.top() + vp.min.y;
        let num_x = r.left() + vp.min.x;
        let cells = p.with_clip_rect(Rect::from_min_max(pos2(num_x + gutter + 1.0, head_y + row_h), clip.max).intersect(clip));
        let nums = p.with_clip_rect(Rect::from_min_max(pos2(clip.left(), head_y + row_h), clip.max).intersect(clip));
        let heads = p.with_clip_rect(Rect::from_min_max(pos2(num_x + gutter + 1.0, clip.top()), clip.max).intersect(clip));
        // Visible columns.
        let mut xs = Vec::with_capacity(widths.len() + 1);
        let mut x = r.left() + gutter;
        for w in widths {
            xs.push(x);
            x += w;
        }
        xs.push(x);
        let c0 = xs.partition_point(|&x| x < r.left() + vp.min.x) .saturating_sub(1);
        let c1 = xs.partition_point(|&x| x < r.left() + vp.max.x).min(t.cols);
        let first = ((vp.min.y / row_h).floor() as usize).min(n);
        let last = (((vp.max.y / row_h).ceil() as usize) + 1).min(n);
        let hover = resp.hover_pos();
        // Cells under the mouse: a press picks one (a row by its number, a column by its
        // title), Shift stretches what is picked, a drag stretches it as it goes.
        if !t.rows.is_empty() {
            let (pressed, down, shift, origin, pos) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.modifiers.shift, i.pointer.press_origin(), i.pointer.interact_pos()));
            let (rows, cols) = (t.rows.len(), t.cols);
            let at = |pp: egui::Pos2| {
                let row = (((pp.y - r.top()) / row_h).floor() as isize - 1).clamp(0, rows as isize - 1) as usize;
                let col = xs.partition_point(|&x| x <= pp.x).saturating_sub(1).min(cols - 1);
                (row, col, pp.y < head_y + row_h, pp.x < num_x + gutter)
            };
            let on = resp.is_pointer_button_down_on();
            if let Some(o) = origin.filter(|_| pressed && on) {
                let (row, col, head, nums) = at(o);
                match sel.as_mut() {
                    Some(s) if shift => {
                        s.head = match grab {
                            Grab::Rows => (row, cols - 1),
                            Grab::Cols => (rows - 1, col),
                            _ => (row, col),
                        }
                    }
                    _ => {
                        let (g, a, b) = match (head, nums) {
                            (true, true) => (Grab::All, (0, 0), (rows - 1, cols - 1)),
                            (true, false) => (Grab::Cols, (0, col), (rows - 1, col)),
                            (false, true) => (Grab::Rows, (row, 0), (row, cols - 1)),
                            (false, false) => (Grab::Cells, (row, col), (row, col)),
                        };
                        grab = g;
                        sel = Some(crate::textsel::Sel::new(a, b));
                    }
                }
            } else if let (true, true, Some(pp)) = (on, down, pos) {
                let (row, col, ..) = at(pp);
                if let Some(s) = sel.as_mut() {
                    s.head = match grab {
                        Grab::Rows => (row, cols - 1),
                        Grab::Cols => (rows - 1, col),
                        Grab::All => s.head,
                        Grab::Cells => (row, col),
                    };
                }
                crate::textsel::autoscroll(ui, pp);
            }
        }
        let picked = corners(sel);
        let mut tip: Option<String> = None;
        let cell_text = |ui: &Ui, painter: &egui::Painter, s: &str, x: f32, w: f32, y: f32, right: bool, col: Color32| -> bool {
            let s = one_line(s);
            let g = crate::listing::elided(ui, &s, font.clone(), w - pad * 2.0, col);
            let cut = g.size().x + 0.5 < ui.fonts_mut(|f| s.chars().map(|c| f.glyph_width(&font, c)).sum::<f32>()) - 0.5;
            let gx = if right { x + w - pad - g.size().x } else { x + pad };
            painter.galley(pos2(gx, y + lift), g, col);
            cut
        };
        for i in first..last {
            let y = r.top() + (i + 1) as f32 * row_h;
            if i == t.rows.len() {
                if let Some(note) = &t.note {
                    nums.text(pos2(num_x + gutter + pad, y + lift), Align2::LEFT_TOP, note, font.clone(), pal.text_dim);
                }
                continue;
            }
            let row = &t.rows[i];
            let row_r = Rect::from_min_max(pos2(num_x, y), pos2(r.right().max(num_x + vp.width()), y + row_h));
            // The row under the pointer, under the selection's block (drawn with the
            // first row of it on screen).
            if hover.is_some_and(|h| row_r.contains(h) && h.y > head_y + row_h) {
                match picked.filter(|((r0, _), (r1, _))| (*r0..=*r1).contains(&i) && i > (*r0).max(first)) {
                    // Beside the block, not over it.
                    Some(((_, c0), (_, c1))) => {
                        nums.rect_filled(Rect::from_min_max(row_r.min, pos2(xs[c0], row_r.bottom())), CornerRadius::ZERO, pal.hover);
                        nums.rect_filled(Rect::from_min_max(pos2(xs[(c1 + 1).min(t.cols)], row_r.top()), row_r.max), CornerRadius::ZERO, pal.hover);
                    }
                    None => {
                        nums.rect_filled(row_r, CornerRadius::ZERO, pal.hover);
                    }
                }
            }
            // The selected cells as one block from its first row on screen, so rows
            // painted apart leave no seam between them.
            if let Some(((r0, c0), (r1, c1))) = picked.filter(|((r0, _), (r1, _))| i == (*r0).max(first) && i <= *r1) {
                let bottom = r.top() + (r1.min(last.saturating_sub(1)) + 2) as f32 * row_h;
                let _ = r0;
                crate::textsel::paint(&cells, Rect::from_min_max(pos2(xs[c0], y), pos2(xs[(c1 + 1).min(t.cols)], bottom)), pal);
            }
            for c in c0..c1 {
                let Some(s) = row.get(c) else { continue };
                if s.is_empty() {
                    continue;
                }
                let cut = cell_text(ui, &cells, s, xs[c], widths[c], y, t.numeric[c], pal.text);
                if cut && hover.is_some_and(|h| h.x >= xs[c] && h.x < xs[c + 1] && h.y >= y && h.y < y + row_h && h.x > num_x + gutter) {
                    tip = Some(s.clone());
                }
            }
            nums.text(pos2(num_x + gutter - 8.0, y + lift), Align2::RIGHT_TOP, fmt_count(i + 1), font.clone(), pal.text_dim.gamma_multiply(0.7));
        }
        // The header, drawn last so rows scroll under it.
        let hr = Rect::from_min_max(pos2(clip.left(), head_y), pos2(clip.right(), head_y + row_h));
        p.rect_filled(hr, CornerRadius::ZERO, pal.card());
        p.line_segment([hr.left_bottom(), hr.right_bottom()], Stroke::new(1.0, pal.card_line()));
        for c in c0..c1 {
            let title = t.header.as_ref().and_then(|h| h.get(c)).cloned().unwrap_or_else(|| letters(c));
            let col = if t.header.is_some() { pal.text } else { pal.text_dim };
            if cell_text(ui, &heads, &title, xs[c], widths[c], head_y, t.numeric[c], col) && hover.is_some_and(|h| h.x >= xs[c] && h.x < xs[c + 1] && h.y >= head_y && h.y < head_y + row_h) {
                tip = Some(title);
            }
            if c + 1 < t.cols {
                heads.line_segment([pos2(xs[c + 1], head_y + 5.0), pos2(xs[c + 1], head_y + row_h - 5.0)], Stroke::new(1.0, pal.card_line()));
            }
        }
        p.line_segment([pos2(num_x + gutter, head_y), pos2(num_x + gutter, clip.bottom())], Stroke::new(1.0, pal.card_line()));
        if let Some(s) = tip {
            resp.on_hover_text(s);
        }
    });
    (v.sel, v.grab) = (sel, grab);
}
