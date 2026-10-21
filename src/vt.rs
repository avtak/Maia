//! The terminal pane's screen: what the programs running in it drew, kept as rows of
//! packed cells with a bounded scrollback. `vte` splits their output into text and
//! control sequences; `Screen` carries out the part of xterm that shells and full-screen
//! programs (vim, htop, less) rely on.

use std::collections::VecDeque;
use std::sync::Arc;

use vte::{Params, Perform};

/// Rows kept above the screen.
pub const SCROLLBACK: usize = 5000;

// Attributes, kept in a cell's character above its 21 bits.
pub const BOLD: u32 = 1 << 21;
pub const DIM: u32 = 1 << 22;
pub const ITALIC: u32 = 1 << 23;
pub const UNDERLINE: u32 = 1 << 24;
pub const INVERSE: u32 = 1 << 25;
pub const STRIKE: u32 = 1 << 26;
pub const HIDDEN: u32 = 1 << 27;
/// The left half of a wide character; the right half is a `SPACER`.
pub const WIDE: u32 = 1 << 28;
pub const SPACER: u32 = 1 << 29;
/// Part of a hyperlink (OSC 8): its address is in the row's `links`.
pub const LINK: u32 = 1 << 30;
pub const STYLE: u32 = BOLD | DIM | ITALIC | UNDERLINE | INVERSE | STRIKE | HIDDEN;
const CHAR: u32 = (1 << 21) - 1;

/// A cell's colour: 0–255 from the xterm palette, the theme's own, or `RGB` with five
/// bits a channel.
pub const DEFAULT: u16 = 256;
pub const RGB: u16 = 0x8000;

pub fn rgb(r: u16, g: u16, b: u16) -> u16 {
    RGB | (r.min(255) >> 3) << 10 | (g.min(255) >> 3) << 5 | b.min(255) >> 3
}

/// One character cell in 8 bytes: the character with its attributes, and its colours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: u32,
    pub fg: u16,
    pub bg: u16,
}

impl Cell {
    pub const BLANK: Cell = Cell { c: ' ' as u32, fg: DEFAULT, bg: DEFAULT };

    pub fn ch(self) -> char {
        char::from_u32(self.c & CHAR).unwrap_or(' ')
    }

    pub fn flags(self) -> u32 {
        self.c & !CHAR
    }

    /// Nothing to see: a space with no background or line through it.
    fn empty(self) -> bool {
        self.c & (CHAR | INVERSE | UNDERLINE | STRIKE) == ' ' as u32 && self.bg == DEFAULT
    }
}

#[derive(Clone, Default, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The text runs on into the next row (it reached the edge).
    pub wrapped: bool,
    /// Hyperlinks over columns `from..to`. A cell is linked only while it is marked
    /// `LINK` too, so text written over one or erased leaves it.
    pub links: Vec<(u16, u16, Arc<str>)>,
}

impl Row {
    fn blank(cols: usize) -> Row {
        Row { cells: vec![Cell::BLANK; cols], ..Row::default() }
    }

    /// The address of the hyperlink at column `x`.
    pub fn link(&self, x: usize) -> Option<&Arc<str>> {
        let c = self.cells.get(x)?;
        if c.flags() & LINK == 0 {
            return None;
        }
        self.links.iter().rev().find(|l| (l.0 as usize..l.1 as usize).contains(&x)).map(|l| &l.2)
    }
}

/// What a program asked to hear of the mouse (xterm's modes 9, 1000, 1002, 1003).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Track {
    #[default]
    Off,
    Press,
    Click,
    Drag,
    Motion,
}

/// How a mouse report is written: xterm's bytes, UTF-8 (1005), SGR (1006), urxvt (1015).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Enc {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
}

/// A mouse event to report: a button (0 left, 1 middle, 2 right) pressed or let go, the
/// pointer moving with one held (or none), the wheel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mouse {
    Press(u8),
    Release(u8),
    Motion(Option<u8>),
    Wheel(bool),
}

#[derive(Clone, Copy)]
struct Cursor {
    x: usize,
    y: usize,
    /// Attributes and colours of what is written next (its character unused).
    pen: Cell,
    /// Line drawing characters in place of `` ` `` to `~` (DEC special graphics).
    lines: bool,
}

impl Default for Cursor {
    fn default() -> Cursor {
        Cursor { x: 0, y: 0, pen: Cell::BLANK, lines: false }
    }
}

/// The cursor's look a program asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Block,
    Underline,
    Bar,
}

pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    /// The scrollback, then the screen's rows (the last `rows`, each `cols` wide).
    lines: VecDeque<Row>,
    /// Rows dropped off the top of the scrollback: added to an index into `lines`, a
    /// row's lasting number (a selection stays on its text while output scrolls).
    pub dropped: u64,
    /// The main screen's rows and cursor while the alternate one is shown.
    main: Option<(VecDeque<Row>, Cursor)>,
    cur: Cursor,
    saved: Cursor,
    /// The cursor is past the last column: the next character starts a new row.
    wrap_next: bool,
    /// The scrolling region, first and last row.
    top: usize,
    bottom: usize,
    pub cursor_shown: bool,
    pub shape: Shape,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    autowrap: bool,
    insert: bool,
    origin: bool,
    pub title: String,
    /// The folder the shell said it is in (OSC 7), when it says.
    pub cwd: Option<String>,
    /// Answers to the program's questions (where the cursor is, what terminal this is),
    /// to write back to it.
    pub replies: Vec<u8>,
    /// How many rows the view is scrolled back.
    pub scroll: usize,
    last: char,
    /// The hyperlink what is written next belongs to.
    link: Option<Arc<str>>,
    pub mouse: Track,
    pub mouse_enc: Enc,
    /// Counts changes to what is shown: a search of the text is current while it holds.
    pub changes: u64,
}

/// Where a character of a line of text is on the screen (see `Screen::line_text`).
#[derive(Clone, Copy, Debug)]
pub struct Spot {
    pub byte: u32,
    pub n: u64,
    pub x: u16,
    pub w: u8,
}

/// Text on the screen from one cell to another, both included (lasting row, column).
pub type Span = ((u64, usize), (u64, usize));

/// DEC special graphics for `` ` `` to `~`: box drawing as ncurses uses it.
const LINES: [char; 31] = [
    '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼', '⎺', '⎻', '─', '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£', '·',
];

/// Combining marks and joiners: drawn with the character before them, which is all the
/// grid can do with them.
fn zero_width(c: char) -> bool {
    matches!(c as u32, 0x300..=0x36F | 0x200B..=0x200F | 0xFE00..=0xFE0F | 0x20D0..=0x20FF | 0x7F)
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        let (cols, rows) = (cols.max(2), rows.max(1));
        Screen {
            cols,
            rows,
            lines: (0..rows).map(|_| Row::blank(cols)).collect(),
            dropped: 0,
            main: None,
            cur: Cursor::default(),
            saved: Cursor::default(),
            wrap_next: false,
            top: 0,
            bottom: rows - 1,
            cursor_shown: true,
            shape: Shape::Block,
            app_cursor: false,
            bracketed_paste: false,
            autowrap: true,
            insert: false,
            origin: false,
            title: String::new(),
            cwd: None,
            replies: vec![],
            scroll: 0,
            last: ' ',
            link: None,
            mouse: Track::Off,
            mouse_enc: Enc::X10,
            changes: 0,
        }
    }

    /// The alternate screen is shown (a full-screen program): no scrollback.
    pub fn alt(&self) -> bool {
        self.main.is_some()
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cur.x, self.cur.y)
    }

    /// Rows in the scrollback.
    pub fn history(&self) -> usize {
        self.lines.len() - self.rows
    }

    /// The lasting number of the view's row `y` (see `dropped`).
    pub fn view_line(&self, y: usize) -> u64 {
        self.dropped + (self.history() - self.scroll + y) as u64
    }

    /// A row by its lasting number, while it is kept.
    pub fn line(&self, n: u64) -> Option<&Row> {
        self.lines.get(usize::try_from(n.checked_sub(self.dropped)?).ok()?)
    }

    /// Scroll the view back (positive) or toward the newest rows.
    pub fn scroll_view(&mut self, rows: isize) {
        self.scroll = self.scroll.saturating_add_signed(rows).min(self.history());
    }

    /// The view's row `y` as text, without the spaces after it.
    #[cfg(test)]
    pub fn row_text(&self, y: usize) -> String {
        let n = self.view_line(y);
        self.text((n, 0), (n, self.cols))
    }

    /// The text from `a` to `b` (lasting row number and column, both ends included), a
    /// line break after each row except where the text ran on.
    pub fn text(&self, a: (u64, usize), b: (u64, usize)) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut s = String::new();
        for n in a.0..=b.0 {
            let Some(row) = self.line(n) else { continue };
            let from = if n == a.0 { a.1 } else { 0 };
            let to = if n == b.0 { b.1 + 1 } else { usize::MAX };
            let start = s.len();
            s.extend(row.cells.iter().take(to).skip(from).filter(|c| c.flags() & SPACER == 0).map(|c| c.ch()));
            if !row.wrapped || n == b.0 {
                s.truncate(start + s[start..].trim_end().len());
                if n != b.0 {
                    s.push('\n');
                }
            }
        }
        s
    }

    /// The first and last rows of the line row `n` is part of: rows that ran on into the
    /// next are one line (at most 64 rows each way, which no real line needs).
    pub fn logical(&self, n: u64) -> (u64, u64) {
        let wrapped = |k: u64| self.line(k).is_some_and(|r| r.wrapped);
        let (mut a, mut b) = (n, n);
        while a > self.dropped && n - a < 64 && wrapped(a - 1) {
            a -= 1;
        }
        while b - n < 64 && wrapped(b) && self.line(b + 1).is_some() {
            b += 1;
        }
        (a, b)
    }

    /// Rows `a..=b` as one line of text, and where each of its characters is: its byte in
    /// the text, its row and column, and how many columns it takes.
    pub fn line_text(&self, a: u64, b: u64, text: &mut String, at: &mut Vec<Spot>) {
        text.clear();
        at.clear();
        for n in a..=b {
            let Some(row) = self.line(n) else { continue };
            for (x, c) in row.cells.iter().enumerate() {
                if c.flags() & SPACER != 0 {
                    continue;
                }
                at.push(Spot { byte: text.len() as u32, n, x: x as u16, w: if c.flags() & WIDE != 0 { 2 } else { 1 } });
                text.push(c.ch());
            }
            // A row kept in the scrollback lost the blanks it ended in, which were text.
            if row.wrapped && n < b {
                for x in row.cells.len()..self.cols {
                    at.push(Spot { byte: text.len() as u32, n, x: x as u16, w: 1 });
                    text.push(' ');
                }
            }
        }
    }

    /// Where `q` occurs in the scrollback and on the screen (a line that ran over several
    /// rows is searched as one), as each match's first and last cell; at most `max`.
    pub fn find(&self, q: &str, case: bool, max: usize, out: &mut Vec<Span>) {
        out.clear();
        let (mut text, mut at, mut found) = (String::new(), vec![], vec![]);
        let end = self.dropped + self.lines.len() as u64;
        let mut n = self.dropped;
        while n < end && out.len() < max {
            let mut b = n;
            while b + 1 < end && b - n < 64 && self.line(b).is_some_and(|r| r.wrapped) {
                b += 1;
            }
            self.line_text(n, b, &mut text, &mut at);
            found.clear();
            crate::findbar::find_in(&text, q, case, &mut found);
            for &(s, e) in &found {
                let first = at.partition_point(|p| p.byte < s);
                let last = at.partition_point(|p| p.byte < e).saturating_sub(1);
                if let (Some(f), Some(l)) = (at.get(first), at.get(last)) {
                    out.push(((f.n, f.x as usize), (l.n, l.x as usize + l.w as usize - 1)));
                }
            }
            n = b + 1;
        }
        out.truncate(max);
    }

    /// The word at a cell, as a double click takes it: letters, digits and what paths and
    /// addresses are written with, or a run of blanks, or the one other character.
    pub fn word(&self, p: (u64, usize)) -> Option<Span> {
        let (a, b) = self.logical(p.0);
        let (mut text, mut at) = (String::new(), vec![]);
        self.line_text(a, b, &mut text, &mut at);
        let chars: Vec<char> = text.chars().collect();
        let i = at.iter().position(|s| s.n == p.0 && (s.x as usize..s.x as usize + s.w as usize).contains(&p.1))?;
        let class = |c: char| if c.is_alphanumeric() || "_-./~:@%+#?&=".contains(c) { 1 } else if c == ' ' { 0 } else { 2 };
        let k = class(chars[i]);
        let (mut s, mut e) = (i, i);
        if k != 2 {
            while s > 0 && class(chars[s - 1]) == k {
                s -= 1;
            }
            while e + 1 < chars.len() && class(chars[e + 1]) == k {
                e += 1;
            }
            // Not the full stop or comma of the sentence it ends.
            while e > i && ".,:;?!".contains(chars[e]) {
                e -= 1;
            }
        }
        Some(((at[s].n, at[s].x as usize), (at[e].n, at[e].x as usize + at[e].w as usize - 1)))
    }

    /// The whole line a cell is on, as a triple click takes it.
    pub fn line_span(&self, n: u64) -> Span {
        let (a, b) = self.logical(n);
        ((a, 0), (b, self.cols - 1))
    }

    /// What to write to the program for a mouse event at column `x`, row `y` (of the
    /// screen) with Shift, Alt and Ctrl as `mods`, when it asked for such events.
    pub fn report(&self, ev: Mouse, x: usize, y: usize, mods: (bool, bool, bool)) -> Option<Vec<u8>> {
        let wanted = match (self.mouse, ev) {
            (Track::Off, _) => false,
            (Track::Press, e) => matches!(e, Mouse::Press(_)),
            (Track::Click, e) => !matches!(e, Mouse::Motion(_)),
            (Track::Drag, Mouse::Motion(b)) => b.is_some(),
            _ => true,
        };
        if !wanted {
            return None;
        }
        let sgr = self.mouse_enc == Enc::Sgr;
        let mut cb = match ev {
            Mouse::Press(b) => b as u32,
            Mouse::Release(b) if sgr => b as u32,
            Mouse::Release(_) => 3,
            Mouse::Motion(b) => 32 + b.map_or(3, |b| b as u32),
            Mouse::Wheel(up) => if up { 64 } else { 65 },
        };
        // X10 compatibility mode tells only which button.
        if self.mouse != Track::Press {
            cb += 4 * mods.0 as u32 + 8 * mods.1 as u32 + 16 * mods.2 as u32;
        }
        let (x, y) = (x as u32 + 1, y as u32 + 1);
        let mut out = b"\x1b[".to_vec();
        match self.mouse_enc {
            Enc::Sgr => out.extend_from_slice(format!("<{cb};{x};{y}{}", if matches!(ev, Mouse::Release(_)) { 'm' } else { 'M' }).as_bytes()),
            Enc::Urxvt => out.extend_from_slice(format!("{};{x};{y}M", cb + 32).as_bytes()),
            Enc::Utf8 => {
                out.push(b'M');
                for v in [cb + 32, x + 32, y + 32] {
                    let mut b = [0; 4];
                    out.extend_from_slice(char::from_u32(v.min(2047))?.encode_utf8(&mut b).as_bytes());
                }
            }
            Enc::X10 => {
                // One byte a number: past column 223 there is no way to say where.
                if x > 223 || y > 223 {
                    return None;
                }
                out.extend_from_slice(&[b'M', (cb + 32) as u8, (x + 32) as u8, (y + 32) as u8]);
            }
        }
        Some(out)
    }

    fn row(&mut self, y: usize) -> &mut Row {
        let i = self.lines.len() - self.rows + y;
        &mut self.lines[i]
    }

    /// An erased cell: the pen's background stays (as xterm's "bce").
    fn blank(&self) -> Cell {
        Cell { bg: self.cur.pen.bg, ..Cell::BLANK }
    }

    /// Erase columns `from..to` of row `y`, and the other half of a wide character cut.
    fn erase(&mut self, y: usize, from: usize, to: usize) {
        let (to, b) = (to.min(self.cols), self.blank());
        if from >= to {
            return;
        }
        self.split_wide(y, from);
        self.split_wide(y, to - 1);
        self.row(y).cells[from..to].fill(b);
    }

    /// Writing over half of a wide character blanks its other half.
    fn split_wide(&mut self, y: usize, x: usize) {
        let cols = self.cols;
        let cells = &mut self.row(y).cells;
        let c = cells[x];
        if c.flags() & SPACER != 0 && x > 0 {
            cells[x - 1] = Cell { c: ' ' as u32 | cells[x - 1].c & STYLE, ..cells[x - 1] };
        } else if c.flags() & WIDE != 0 && x + 1 < cols {
            cells[x + 1] = Cell { c: ' ' as u32 | cells[x + 1].c & STYLE, ..cells[x + 1] };
        }
    }

    fn fresh_row(&self, mut row: Row) -> Row {
        row.cells.clear();
        row.cells.resize(self.cols, self.blank());
        row.wrapped = false;
        row.links.clear();
        row
    }

    /// Scroll the region up `n` rows. When it is the whole main screen, the top rows go
    /// into the scrollback.
    fn scroll_up(&mut self, n: usize) {
        if self.top > 0 || self.bottom + 1 < self.rows || self.alt() {
            return self.shift(self.top, n, true);
        }
        for _ in 0..n.min(self.rows) {
            let i = self.history();
            trim(&mut self.lines[i]);
            let old = if i >= SCROLLBACK {
                self.dropped += 1;
                self.lines.pop_front().unwrap_or_default()
            } else {
                Row::default()
            };
            let row = self.fresh_row(old);
            self.lines.push_back(row);
        }
        // A view scrolled back stays on the rows it shows.
        if self.scroll > 0 {
            self.scroll = (self.scroll + n).min(self.history());
        }
    }

    /// Move rows `top..=self.bottom` up (or down) `n`, blank rows coming in.
    fn shift(&mut self, top: usize, n: usize, up: bool) {
        let (base, bottom) = (self.history(), self.bottom);
        let (from, to) = if up { (base + top, base + bottom) } else { (base + bottom, base + top) };
        for _ in 0..n.min(bottom + 1 - top) {
            let row = self.lines.remove(from).unwrap_or_default();
            let row = self.fresh_row(row);
            self.lines.insert(to, row);
        }
    }

    fn newline(&mut self) {
        if self.cur.y == self.bottom {
            self.scroll_up(1);
        } else if self.cur.y + 1 < self.rows {
            self.cur.y += 1;
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (lo, hi) = if self.origin { (self.top, self.bottom) } else { (0, self.rows - 1) };
        self.cur.x = x.min(self.cols - 1);
        self.cur.y = (y + lo).min(hi);
        self.wrap_next = false;
    }

    /// Back to the cursor saved last (DECRC).
    fn restore(&mut self) {
        self.cur = self.saved;
        self.cur.x = self.cur.x.min(self.cols - 1);
        self.cur.y = self.cur.y.min(self.rows - 1);
        self.wrap_next = false;
    }

    /// Show the alternate screen, blank, or go back to the main one and its cursor.
    fn set_alt(&mut self, on: bool) {
        if on == self.alt() {
            return;
        }
        if on {
            let blank: VecDeque<Row> = (0..self.rows).map(|_| Row::blank(self.cols)).collect();
            self.main = Some((std::mem::replace(&mut self.lines, blank), self.cur));
            self.scroll = 0;
        } else if let Some((lines, cur)) = self.main.take() {
            self.lines = lines;
            self.cur = cur;
            self.wrap_next = false;
        }
    }

    /// Fit to `cols` × `rows`. Rows below the cursor go first, then the top ones go into
    /// the scrollback; growing brings them back. A new width rewraps the main screen's
    /// text, scrollback included; the alternate screen's program draws itself again.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let (old_cols, old_rows) = (self.cols, self.rows);
        self.changes += 1;
        let main = |lines: &mut VecDeque<Row>, cur: &mut Cursor| {
            if cols == old_cols {
                fit(lines, old_rows, rows, cols, &mut cur.y, true);
            } else {
                (cur.x, cur.y) = rewrap(lines, old_rows, rows, old_cols, cols, (cur.x, cur.y));
            }
        };
        if let Some((lines, cur)) = &mut self.main {
            main(lines, cur);
            fit(&mut self.lines, old_rows, rows, cols, &mut self.cur.y, false);
        } else {
            main(&mut self.lines, &mut self.cur);
        }
        (self.cols, self.rows) = (cols, rows);
        self.cur.x = self.cur.x.min(cols - 1);
        self.saved.x = self.saved.x.min(cols - 1);
        self.saved.y = self.saved.y.min(rows - 1);
        (self.top, self.bottom) = (0, rows - 1);
        self.wrap_next = false;
        let over = self.history().saturating_sub(SCROLLBACK);
        if over > 0 {
            self.lines.drain(..over);
            self.dropped += over as u64;
        }
        self.scroll = self.scroll.min(self.history());
    }

    fn reset(&mut self) {
        self.set_alt(false);
        let mut fresh = Screen::new(self.cols, self.rows);
        fresh.lines = std::mem::take(&mut self.lines);
        fresh.dropped = self.dropped;
        fresh.changes = self.changes;
        fresh.replies = std::mem::take(&mut self.replies);
        *self = fresh;
        for y in 0..self.rows {
            self.erase(y, 0, self.cols);
        }
    }

    fn sgr(&mut self, params: &Params) {
        let pen = &mut self.cur.pen;
        if params.is_empty() {
            *pen = Cell::BLANK;
            return;
        }
        let mut it = params.iter();
        while let Some(p) = it.next() {
            let on = |pen: &mut Cell, f: u32| pen.c |= f;
            let off = |pen: &mut Cell, f: u32| pen.c &= !f;
            match p[0] {
                0 => *pen = Cell::BLANK,
                1 => on(pen, BOLD),
                2 => on(pen, DIM),
                3 => on(pen, ITALIC),
                4 if p.get(1) == Some(&0) => off(pen, UNDERLINE),
                4 | 21 => on(pen, UNDERLINE),
                7 => on(pen, INVERSE),
                8 => on(pen, HIDDEN),
                9 => on(pen, STRIKE),
                22 => off(pen, BOLD | DIM),
                23 => off(pen, ITALIC),
                24 => off(pen, UNDERLINE),
                27 => off(pen, INVERSE),
                28 => off(pen, HIDDEN),
                29 => off(pen, STRIKE),
                n @ 30..=37 => pen.fg = n - 30,
                38 => pen.fg = color(p, &mut it).unwrap_or(pen.fg),
                39 => pen.fg = DEFAULT,
                n @ 40..=47 => pen.bg = n - 40,
                48 => pen.bg = color(p, &mut it).unwrap_or(pen.bg),
                49 => pen.bg = DEFAULT,
                n @ 90..=97 => pen.fg = n - 90 + 8,
                n @ 100..=107 => pen.bg = n - 100 + 8,
                _ => {}
            }
        }
    }

    fn mode(&mut self, private: bool, params: &Params, on: bool) {
        for p in params.iter() {
            match (private, p[0]) {
                (true, 1) => self.app_cursor = on,
                (true, 6) => {
                    self.origin = on;
                    self.goto(0, 0);
                }
                (true, 7) => self.autowrap = on,
                (true, 25) => self.cursor_shown = on,
                (true, 47 | 1047 | 1049) => self.set_alt(on),
                (true, 2004) => self.bracketed_paste = on,
                (true, m @ (9 | 1000 | 1002 | 1003)) => {
                    self.mouse = match (on, m) {
                        (false, _) => Track::Off,
                        (_, 9) => Track::Press,
                        (_, 1000) => Track::Click,
                        (_, 1002) => Track::Drag,
                        _ => Track::Motion,
                    }
                }
                (true, m @ (1005 | 1006 | 1015)) => {
                    let e = match m {
                        1005 => Enc::Utf8,
                        1006 => Enc::Sgr,
                        _ => Enc::Urxvt,
                    };
                    if on {
                        self.mouse_enc = e;
                    } else if self.mouse_enc == e {
                        self.mouse_enc = Enc::X10;
                    }
                }
                (false, 4) => self.insert = on,
                _ => {}
            }
        }
    }
}

/// Keep only what shows of a row leaving the screen for the scrollback.
fn trim(row: &mut Row) {
    while row.cells.last().is_some_and(|c| c.empty()) {
        row.cells.pop();
    }
    row.cells.shrink_to_fit();
}

/// Fit `lines`, the last `old` of them on screen, to `rows` rows of `cols` columns;
/// `keep`: rows leave for the scrollback (the main screen) rather than go.
fn fit(lines: &mut VecDeque<Row>, old: usize, rows: usize, cols: usize, y: &mut usize, keep: bool) {
    if rows < old {
        let below = (old - 1 - (*y).min(old - 1)).min(old - rows);
        lines.truncate(lines.len() - below);
        let up = old - rows - below;
        *y -= up.min(*y);
        if keep {
            let n = lines.len();
            lines.range_mut(n - rows - up..n - rows).for_each(trim);
        } else {
            lines.drain(..up);
        }
    } else if rows > old {
        let back = if keep { (lines.len() - old).min(rows - old) } else { 0 };
        *y += back;
        lines.extend((0..rows - old - back).map(|_| Row::default()));
    }
    let n = lines.len();
    for r in lines.range_mut(n - rows..) {
        r.cells.resize(cols, Cell::BLANK);
        if let Some(c) = r.cells.last_mut().filter(|c| c.flags() & WIDE != 0) {
            *c = Cell::BLANK;
        }
    }
}

/// Rewrap the main screen, `old_rows` of `old_cols` at the end of `lines`, to `rows` of
/// `cols`: rows that ran on join into the line they were written as, which is cut again
/// at the new width. The cursor (column, row) keeps its place in its line; blank rows
/// under it are room, not text, and go first. Returns where the cursor is now.
fn rewrap(lines: &mut VecDeque<Row>, old_rows: usize, rows: usize, old_cols: usize, cols: usize, cur: (usize, usize)) -> (usize, usize) {
    let at = lines.len() - old_rows + cur.1;
    while lines.len() > at + 1 && lines.back().is_some_and(|r| !r.wrapped && r.cells.iter().all(|c| c.empty())) {
        lines.pop_back();
    }
    let old: Vec<Row> = lines.drain(..).collect();
    let (mut line, mut mark, mut pos) = (Vec::<Cell>::new(), None, (0, 0));
    // The line's hyperlinks by where they are in it, cut again with its rows.
    let mut links: Vec<(usize, usize, Arc<str>)> = vec![];
    let cut = |links: &[(usize, usize, Arc<str>)], from: usize, to: usize| -> Vec<(u16, u16, Arc<str>)> {
        links.iter().filter(|l| l.0 < to && l.1 > from).map(|l| ((l.0.max(from) - from) as u16, (l.1.min(to) - from) as u16, l.2.clone())).collect()
    };
    for (i, row) in old.iter().enumerate() {
        if i == at {
            mark = Some(line.len() + cur.0);
        }
        let base = line.len();
        links.extend(row.links.iter().map(|l| (base + l.0 as usize, base + l.1 as usize, l.2.clone())));
        line.extend_from_slice(&row.cells);
        if row.wrapped && i + 1 < old.len() {
            // A row kept in the scrollback lost its trailing blanks, which were text.
            line.resize(line.len() + old_cols.saturating_sub(row.cells.len()), Cell::BLANK);
            // A wide character that did not fit at the edge left it blank and went on.
            if old[i + 1].cells.first().is_some_and(|c| c.flags() & WIDE != 0) && line.last().is_some_and(|c| c.empty()) {
                line.pop();
            }
            continue;
        }
        while line.last().is_some_and(|c| c.empty()) {
            line.pop();
        }
        let (mut row, mut start) = (Vec::with_capacity(cols), 0);
        for (j, &c) in line.iter().enumerate() {
            let w = if c.flags() & WIDE != 0 { 2 } else { 1 };
            if row.len() + w > cols && c.flags() & SPACER == 0 {
                row.resize(cols, Cell::BLANK);
                lines.push_back(Row { cells: std::mem::replace(&mut row, Vec::with_capacity(cols)), wrapped: true, links: cut(&links, start, j) });
                start = j;
            }
            if mark == Some(j) {
                pos = (row.len(), lines.len());
            }
            row.push(c);
        }
        if let Some(m) = mark.filter(|&m| m >= line.len()) {
            pos = ((row.len() + m - line.len()).min(cols - 1), lines.len());
        }
        mark = None;
        lines.push_back(Row { cells: row, wrapped: false, links: cut(&links, start, line.len()) });
        line.clear();
        links.clear();
    }
    // Text under the cursor stays on the screen with it, as much as fits.
    lines.truncate((pos.1 + rows).min(lines.len()));
    while lines.len() < rows {
        lines.push_back(Row::default());
    }
    let top = lines.len() - rows;
    lines.range_mut(..top).for_each(trim);
    lines.range_mut(top..).for_each(|r| r.cells.resize(cols, Cell::BLANK));
    (pos.0, pos.1 - top)
}

/// An extended colour after 38 or 48: `5;n` (palette) or `2;r;g;b`, each also written
/// with colons.
fn color(p: &[u16], it: &mut vte::ParamsIter) -> Option<u16> {
    if p.len() > 1 {
        return match (p[1], p.len()) {
            (5, 3..) => Some(p[2].min(255)),
            (2, 5..) => {
                let c = &p[p.len() - 3..];
                Some(rgb(c[0], c[1], c[2]))
            }
            _ => None,
        };
    }
    match it.next()?[0] {
        5 => Some(it.next()?[0].min(255)),
        2 => {
            let (r, g, b) = (it.next()?[0], it.next()?[0], it.next()?[0]);
            Some(rgb(r, g, b))
        }
        _ => None,
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        let c = match c {
            '`'..='~' if self.cur.lines => LINES[c as usize - 0x60],
            c if zero_width(c) => return,
            c => c,
        };
        let w = crate::inspector::char_cols(c);
        if self.wrap_next && self.autowrap {
            self.row(self.cur.y).wrapped = true;
            self.cur.x = 0;
            self.newline();
        }
        self.wrap_next = false;
        if w == 2 && self.cur.x + 1 >= self.cols {
            if self.autowrap {
                let y = self.cur.y;
                self.erase(y, self.cur.x, self.cols);
                self.row(y).wrapped = true;
                self.cur.x = 0;
                self.newline();
            } else {
                self.cur.x = self.cols - 2;
            }
        }
        let (x, y, pen) = (self.cur.x, self.cur.y, self.cur.pen);
        if self.insert {
            let b = self.blank();
            let cols = self.cols;
            let cells = &mut self.row(y).cells;
            for _ in 0..w {
                cells.insert(x, b);
            }
            cells.truncate(cols);
        }
        self.split_wide(y, x);
        if w == 2 {
            self.split_wide(y, x + 1);
        }
        let style = pen.c & STYLE | if self.link.is_some() { LINK } else { 0 };
        let cells = &mut self.row(y).cells;
        cells[x] = Cell { c: c as u32 | style | if w == 2 { WIDE } else { 0 }, ..pen };
        if w == 2 {
            cells[x + 1] = Cell { c: ' ' as u32 | style | SPACER, ..pen };
        }
        if let Some(l) = self.link.clone() {
            let (a, b) = (x as u16, (x + w) as u16);
            let links = &mut self.row(y).links;
            match links.last_mut() {
                Some(last) if Arc::ptr_eq(&last.2, &l) && last.1 == a => last.1 = b,
                _ => {
                    // A link written over another cuts it back to what is left of it.
                    for o in links.iter_mut().filter(|o| o.0 < b && o.1 > a) {
                        o.1 = if o.0 < a { a } else { o.0 };
                    }
                    links.retain(|o| o.0 < o.1);
                    links.push((a, b, l));
                }
            }
        }
        self.last = c;
        if x + w >= self.cols {
            self.cur.x = self.cols - 1;
            self.wrap_next = true;
        } else {
            self.cur.x = x + w;
        }
    }

    fn execute(&mut self, b: u8) {
        match b {
            0x08 => {
                self.cur.x = self.cur.x.saturating_sub(1);
                self.wrap_next = false;
            }
            0x09 => self.cur.x = ((self.cur.x / 8 + 1) * 8).min(self.cols - 1),
            0x0A..=0x0C => self.newline(),
            0x0D => {
                self.cur.x = 0;
                self.wrap_next = false;
            }
            0x84 => self.newline(),
            0x85 => {
                self.cur.x = 0;
                self.newline();
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut ps = [0u16; 8];
        for (i, p) in params.iter().take(8).enumerate() {
            ps[i] = p[0];
        }
        // A count: missing or 0 means 1.
        let n = |i: usize| ps[i].max(1) as usize;
        let (x, y) = (self.cur.x, self.cur.y);
        match (inter, action) {
            ([], 'm') => self.sgr(params),
            ([], '@') => {
                let (b, cols) = (self.blank(), self.cols);
                self.split_wide(y, x);
                let cells = &mut self.row(y).cells;
                for _ in 0..n(0).min(cols - x) {
                    cells.insert(x, b);
                }
                cells.truncate(cols);
            }
            ([], 'A') => {
                let lo = if y >= self.top { self.top } else { 0 };
                self.cur.y = y.saturating_sub(n(0)).max(lo);
                self.wrap_next = false;
            }
            ([], 'B' | 'e') => {
                let hi = if y <= self.bottom { self.bottom } else { self.rows - 1 };
                self.cur.y = (y + n(0)).min(hi);
                self.wrap_next = false;
            }
            ([], 'C' | 'a') => {
                self.cur.x = (x + n(0)).min(self.cols - 1);
                self.wrap_next = false;
            }
            ([], 'D') => {
                self.cur.x = x.saturating_sub(n(0));
                self.wrap_next = false;
            }
            ([], 'E') => {
                let hi = if y <= self.bottom { self.bottom } else { self.rows - 1 };
                self.cur.y = (y + n(0)).min(hi);
                self.cur.x = 0;
                self.wrap_next = false;
            }
            ([], 'F') => {
                let lo = if y >= self.top { self.top } else { 0 };
                self.cur.y = y.saturating_sub(n(0)).max(lo);
                self.cur.x = 0;
                self.wrap_next = false;
            }
            ([], 'G' | '`') => {
                self.cur.x = (n(0) - 1).min(self.cols - 1);
                self.wrap_next = false;
            }
            ([], 'H' | 'f') => self.goto(n(1) - 1, n(0) - 1),
            ([], 'd') => {
                let x = self.cur.x;
                self.goto(x, n(0) - 1);
            }
            ([], 'J') => {
                let (cols, rows) = (self.cols, self.rows);
                match ps[0] {
                    0 => {
                        self.erase(y, x, cols);
                        (y + 1..rows).for_each(|r| self.erase(r, 0, cols));
                    }
                    1 => {
                        (0..y).for_each(|r| self.erase(r, 0, cols));
                        self.erase(y, 0, x + 1);
                    }
                    2 => (0..rows).for_each(|r| self.erase(r, 0, cols)),
                    3 if !self.alt() => {
                        let h = self.history();
                        self.lines.drain(..h);
                        self.dropped += h as u64;
                        self.scroll = 0;
                    }
                    _ => {}
                }
            }
            ([], 'K') => match ps[0] {
                0 => self.erase(y, x, self.cols),
                1 => self.erase(y, 0, x + 1),
                2 => self.erase(y, 0, self.cols),
                _ => {}
            },
            ([], 'L' | 'M') if (self.top..=self.bottom).contains(&y) => {
                self.shift(y, n(0), action == 'M');
                self.cur.x = 0;
                self.wrap_next = false;
            }
            ([], 'P') => {
                let (b, cols) = (self.blank(), self.cols);
                self.split_wide(y, x);
                let cells = &mut self.row(y).cells;
                cells.drain(x..(x + n(0)).min(cols));
                cells.resize(cols, b);
            }
            ([], 'X') => self.erase(y, x, x + n(0)),
            ([], 'S') => self.scroll_up(n(0)),
            ([], 'T') => self.shift(self.top, n(0), false),
            ([], 'b') => {
                let c = self.last;
                for _ in 0..n(0).min(self.cols * self.rows) {
                    self.print(c);
                }
            }
            ([], 'c') if ps[0] == 0 => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            ([b'>'], 'c') if ps[0] == 0 => self.replies.extend_from_slice(b"\x1b[>0;10;1c"),
            ([], 'n') => match ps[0] {
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let row = if self.origin { y - self.top } else { y };
                    self.replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, x + 1).as_bytes());
                }
                _ => {}
            },
            ([], 'r') => {
                let (top, bottom) = (n(0) - 1, if ps[1] == 0 { self.rows } else { ps[1] as usize }.min(self.rows) - 1);
                if top < bottom {
                    (self.top, self.bottom) = (top, bottom);
                    self.goto(0, 0);
                }
            }
            ([], 's') => self.saved = self.cur,
            ([], 'u') => self.restore(),
            ([q], 'h' | 'l') => self.mode(*q == b'?', params, action == 'h'),
            ([], 'h' | 'l') => self.mode(false, params, action == 'h'),
            ([b' '], 'q') => {
                self.shape = match ps[0] {
                    3 | 4 => Shape::Underline,
                    5 | 6 => Shape::Bar,
                    _ => Shape::Block,
                }
            }
            ([b'!'], 'p') => {
                self.cur.pen = Cell::BLANK;
                (self.top, self.bottom) = (0, self.rows - 1);
                (self.insert, self.origin, self.autowrap, self.app_cursor, self.cursor_shown) = (false, false, true, false, true);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, b: u8) {
        if ignore {
            return;
        }
        match (inter, b) {
            ([], b'7') => self.saved = self.cur,
            ([], b'8') => self.restore(),
            ([], b'D') => self.newline(),
            ([], b'E') => {
                self.cur.x = 0;
                self.newline();
            }
            ([], b'M') => {
                if self.cur.y == self.top {
                    self.shift(self.top, 1, false);
                } else {
                    self.cur.y = self.cur.y.saturating_sub(1);
                }
                self.wrap_next = false;
            }
            ([], b'c') => self.reset(),
            ([b'('], b'0') => self.cur.lines = true,
            ([b'('], _) => self.cur.lines = false,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        match params {
            [b"0" | b"2", rest @ ..] => {
                let t = rest.join(&b';');
                self.title = String::from_utf8_lossy(&t).chars().filter(|c| !c.is_control()).take(200).collect();
            }
            [b"8", _, uri @ ..] => {
                let u = uri.join(&b';');
                self.link = (!u.is_empty()).then(|| Arc::from(String::from_utf8_lossy(&u).as_ref()));
            }
            [b"7", uri, ..] => {
                // file://host/path, with %-escapes.
                self.cwd = crate::termlink::file_uri(&String::from_utf8_lossy(uri));
            }
            _ => {}
        }
    }
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The parser with the screen it draws on.
pub struct Term {
    parser: vte::Parser,
    pub screen: Screen,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Term {
        Term { parser: vte::Parser::new(), screen: Screen::new(cols, rows) }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
        self.screen.changes += 1;
    }
}
