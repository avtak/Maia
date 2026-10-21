//! Files too big to read whole: read a window at a time on a worker. Text keeps a sparse
//! index of where every `STEP`th line starts (8 bytes per 1024 lines), so a multi-gigabyte
//! log scrolls anywhere at once; anything that isn't text is shown as hex.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

pub const STEP: u64 = 1024;
/// Longest line kept, in bytes; the rest of it is skipped (a minified file is one line).
const LINE_CAP: usize = 4000;
/// Matches remembered by a search; past it the count says "+".
pub const MAX_HITS: usize = 100_000;

#[derive(Default)]
struct Index {
    /// Where lines 0, STEP, 2·STEP… start.
    marks: Vec<u64>,
    /// Lines counted so far (all of them once `done`).
    lines: u64,
    /// Bytes looked through.
    seen: u64,
    done: bool,
}

enum Req {
    Lines(u64, u64),
    Bytes(u64, usize),
}

pub enum Got {
    /// Lines from the first given.
    Lines(u64, Vec<String>),
    /// Bytes from the offset given.
    Bytes(u64, Vec<u8>),
}

/// A big file being shown: its index, and the window read last.
pub struct Pager {
    pub path: PathBuf,
    pub size: u64,
    pub hex: bool,
    /// Bytes on a hex row (fewer in a narrow preview).
    pub bpr: u64,
    /// Rows drawn last frame.
    pub seen: (usize, usize),
    /// A match to scroll to, kept until its line is read (to scroll sideways to it too).
    pub reveal: Option<u64>,
    /// Highlighting of the window of lines held: (its first line, its length, spans).
    pub spans: Option<(u64, usize, Vec<Vec<(usize, usize, crate::syntax::Kind)>>)>,
    index: Arc<Mutex<Index>>,
    req: Sender<Req>,
    rx: Receiver<Got>,
    pub got: Option<Got>,
    /// What was last asked for, so a frame doesn't ask twice.
    asked: Option<(u64, u64)>,
    /// A search through the whole file, while there is one.
    pub find: Option<Find>,
    /// Text selected, by line and byte as shown; or bytes (both ends taken), and whether
    /// as their text rather than as hex.
    pub sel: Option<crate::textsel::Sel>,
    pub hsel: Option<(crate::textsel::Sel<u64>, bool)>,
}

impl Pager {
    pub fn open(path: &Path, size: u64, hex: bool, ctx: &egui::Context) -> Option<Pager> {
        let f = File::open(path).ok()?;
        let index = Arc::new(Mutex::new(Index { marks: vec![0], done: hex, ..Default::default() }));
        let (req, req_rx) = channel();
        let (tx, rx) = channel();
        let (ix, ctx) = (index.clone(), ctx.clone());
        std::thread::spawn(move || worker(f, size, ix, req_rx, tx, ctx));
        Some(Pager { path: path.to_path_buf(), size, hex, bpr: 16, seen: (0, 0), reveal: None, spans: None, index, req, rx, got: None, asked: None, find: None, sel: None, hsel: None })
    }

    /// Lines known (text), or hex rows; and whether that is all of them.
    pub fn rows(&self) -> (u64, bool) {
        if self.hex {
            return (self.size.div_ceil(self.bpr), true);
        }
        let i = self.index.lock().unwrap();
        (i.lines, i.done)
    }

    /// The lines there will be, guessed from those counted so far (for a gutter that
    /// doesn't widen as the count goes on).
    pub fn estimate(&self) -> u64 {
        let i = self.index.lock().unwrap();
        if i.done || i.seen == 0 { i.lines } else { (i.lines as f64 * self.size as f64 / i.seen as f64) as u64 }
    }

    /// How far the line count has got, from 0 to 1.
    pub fn indexed(&self) -> f32 {
        let i = self.index.lock().unwrap();
        if i.done { 1.0 } else { i.seen as f32 / self.size.max(1) as f32 }
    }

    /// Take what the worker read, and ask for rows `a..b` (lines, or hex rows) when the
    /// window held doesn't cover them.
    pub fn want(&mut self, a: u64, b: u64) {
        while let Ok(g) = self.rx.try_recv() {
            self.got = Some(g);
        }
        let held = match &self.got {
            Some(Got::Lines(f, v)) => (*f, f + v.len() as u64),
            Some(Got::Bytes(o, v)) => (o.div_ceil(self.bpr), (o + v.len() as u64) / self.bpr + u64::from(o + v.len() as u64 == self.size)),
            None => (0, 0),
        };
        let total = self.rows().0;
        let b = b.min(total);
        if a >= held.0 && b <= held.1 || a >= b {
            return;
        }
        // Some rows either side, so a scroll of a few lines needs no read.
        let span = (b - a).max(1);
        let (a, b) = (a.saturating_sub(span), (b + span).min(total));
        if self.asked == Some((a, b)) {
            return;
        }
        self.asked = Some((a, b));
        let _ = self.req.send(if self.hex { Req::Bytes(a * self.bpr, ((b - a) * self.bpr) as usize) } else { Req::Lines(a, b) });
    }

    #[cfg(test)]
    /// Line `i`, when it has been read.
    pub fn line(&self, i: u64) -> Option<&str> {
        match &self.got {
            Some(Got::Lines(f, v)) if i >= *f => v.get((i - f) as usize).map(String::as_str),
            _ => None,
        }
    }

    /// The bytes of hex row `r`, when read.
    pub fn row_bytes(&self, r: u64) -> Option<&[u8]> {
        match &self.got {
            Some(Got::Bytes(o, v)) if r * self.bpr >= *o => {
                let s = (r * self.bpr - o) as usize;
                (s < v.len()).then(|| &v[s..(s + self.bpr as usize).min(v.len())])
            }
            _ => None,
        }
    }

    /// The loaded window of lines: (first, lines).
    pub fn window(&self) -> Option<(u64, &[String])> {
        match &self.got {
            Some(Got::Lines(f, v)) => Some((*f, v)),
            _ => None,
        }
    }

    /// The text selected, read on a worker from the nearest line counted before it, as
    /// the lines are shown; at most `MAX_COPY` bytes of it (then the flag is set).
    pub fn copy_lines(&self, sel: crate::textsel::Sel, ctx: &egui::Context) -> Receiver<(String, bool)> {
        let (tx, rx) = channel();
        let ((l0, _), _) = sel.span();
        let start = self.index.lock().unwrap().marks.get(l0 / STEP as usize).copied();
        let (path, ctx) = (self.path.clone(), ctx.clone());
        std::thread::spawn(move || {
            let got = match (start, File::open(&path)) {
                (Some(start), Ok(f)) => selected_lines(&f, start, l0 - l0 % STEP as usize, &sel, crate::textsel::MAX_COPY as usize),
                _ => (String::new(), false),
            };
            let _ = tx.send(got);
            ctx.request_repaint();
        });
        rx
    }

    /// The bytes selected, read on a worker: as hex pairs, or as the text column shows
    /// them (a dot for what isn't printable); at most `MAX_COPY` bytes read.
    pub fn copy_bytes(&self, sel: crate::textsel::Sel<u64>, text: bool, ctx: &egui::Context) -> Receiver<(String, bool)> {
        let (tx, rx) = channel();
        let (a, b) = sel.span();
        let (path, size, ctx) = (self.path.clone(), self.size, ctx.clone());
        std::thread::spawn(move || {
            let end = b.min(size.saturating_sub(1));
            let cut = end + 1 - a.min(end + 1) > crate::textsel::MAX_COPY / 3;
            let end = if cut { a + crate::textsel::MAX_COPY / 3 - 1 } else { end };
            let mut out = String::new();
            if let Ok(f) = File::open(&path) {
                let mut buf = vec![0u8; 256 << 10];
                let mut at = a;
                while at <= end {
                    let want = ((end + 1 - at) as usize).min(buf.len());
                    let n = f.read_at(&mut buf[..want], at).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    out.push_str(&bytes_text(&buf[..n], text, at == a));
                    at += n as u64;
                }
            }
            let _ = tx.send((out, cut));
            ctx.request_repaint();
        });
        rx
    }

    /// Search the whole file for `q` (text: ignoring ASCII case, hits are lines; hex:
    /// bytes, given as text or as hex pairs, hits are offsets).
    pub fn search(&mut self, q: &str, ctx: &egui::Context) {
        if self.find.as_ref().is_some_and(|f| f.query == q) {
            return;
        }
        self.find = (!q.is_empty()).then(|| Find::start(&self.path, q, self.hex, ctx));
    }
}

fn worker(f: File, size: u64, ix: Arc<Mutex<Index>>, rx: Receiver<Req>, tx: Sender<Got>, ctx: egui::Context) {
    let mut buf = vec![0u8; 1 << 20];
    let (mut at, mut lines) = (0u64, 0u64);
    loop {
        let indexing = !ix.lock().unwrap().done;
        let r = if indexing {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(_) => return,
            }
        } else {
            match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => return,
            }
        };
        match r {
            Some(Req::Bytes(off, len)) => {
                let mut v = vec![0u8; len.min(size.saturating_sub(off) as usize)];
                let n = f.read_at(&mut v, off).unwrap_or(0);
                v.truncate(n);
                if tx.send(Got::Bytes(off, v)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            Some(Req::Lines(a, b)) => {
                let start = ix.lock().unwrap().marks.get((a / STEP) as usize).copied();
                let Some(start) = start else { continue };
                if tx.send(Got::Lines(a, read_lines(&f, start, a % STEP, b - a))).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            None => {
                // A step of the line count, between requests.
                let n = f.read_at(&mut buf, at).unwrap_or(0);
                let mut g = ix.lock().unwrap();
                if n == 0 {
                    // A last line without a line break still counts.
                    let mut last = [b'\n'];
                    let _ = f.read_exact_at(&mut last, at.saturating_sub(1));
                    g.lines = lines + u64::from(at > 0 && last[0] != b'\n');
                    g.done = true;
                    g.seen = size;
                    drop(g);
                    ctx.request_repaint();
                    continue;
                }
                for (i, _) in buf[..n].iter().enumerate().filter(|(_, b)| **b == b'\n') {
                    lines += 1;
                    if lines % STEP == 0 {
                        g.marks.push(at + i as u64 + 1);
                    }
                }
                at += n as u64;
                g.lines = lines;
                g.seen = at;
                drop(g);
                // Shown as it grows, a few times a second rather than every megabyte.
                if at % (32 << 20) < n as u64 {
                    ctx.request_repaint();
                }
            }
        }
    }
}

/// `n` lines from byte `start`, after skipping `skip` lines.
fn read_lines(f: &File, start: u64, skip: u64, n: u64) -> Vec<String> {
    let mut out = Vec::with_capacity(n as usize);
    let mut buf = vec![0u8; 256 << 10];
    let (mut at, mut skip) = (start, skip);
    let mut line: Vec<u8> = vec![];
    while (out.len() as u64) < n {
        let got = f.read_at(&mut buf, at).unwrap_or(0);
        if got == 0 {
            if !line.is_empty() && skip == 0 {
                out.push(line_text(&line));
            }
            break;
        }
        at += got as u64;
        let mut rest = &buf[..got];
        while !rest.is_empty() && (out.len() as u64) < n {
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if skip > 0 {
                        skip -= 1;
                    } else {
                        line.extend_from_slice(&rest[..i.min(LINE_CAP.saturating_sub(line.len()))]);
                        out.push(line_text(&line));
                    }
                    line.clear();
                    rest = &rest[i + 1..];
                }
                None => {
                    if skip == 0 {
                        line.extend_from_slice(&rest[..rest.len().min(LINE_CAP.saturating_sub(line.len()))]);
                    }
                    rest = &[];
                }
            }
        }
    }
    out
}

/// Bytes as the hex view's columns show them: pairs of hex digits with a space between
/// (`first`: none before the first), or their text, a dot for each byte not printable.
pub fn bytes_text(b: &[u8], text: bool, first: bool) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * if text { 1 } else { 3 });
    for (i, &c) in b.iter().enumerate() {
        if text {
            s.push(if (32..127).contains(&c) { c as char } else { '.' });
        } else {
            if i > 0 || !first {
                s.push(' ');
            }
            let _ = write!(s, "{c:02x}");
        }
    }
    s
}

/// The selected part of the lines from byte `start`, where line `first` begins: each as
/// `line_text` makes it, so the bytes the selection counts are the ones shown.
pub fn selected_lines(f: &File, start: u64, first: usize, sel: &crate::textsel::Sel, max: usize) -> (String, bool) {
    let ((l0, _), (l1, _)) = sel.span();
    let mut out = String::new();
    let mut buf = vec![0u8; 256 << 10];
    let (mut at, mut li) = (start, first);
    let mut line: Vec<u8> = vec![];
    let size = f.metadata().map_or(0, |m| m.len());
    let take = |li: usize, line: &[u8], out: &mut String, last: bool| -> bool {
        if li >= l0 {
            let t = line_text(line);
            if let Some((a, b, nl)) = sel.on_line(li, t.len()) {
                out.push_str(t.get(a..b).unwrap_or(""));
                if nl && !last {
                    out.push('\n');
                }
            }
        }
        li >= l1 || out.len() >= max
    };
    loop {
        let got = f.read_at(&mut buf, at).unwrap_or(0);
        if got == 0 {
            if !line.is_empty() {
                take(li, &line, &mut out, true);
            }
            break;
        }
        at += got as u64;
        let mut rest = &buf[..got];
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            if line.len() < LINE_CAP {
                line.extend_from_slice(&rest[..i.min(LINE_CAP - line.len())]);
            }
            let end = at - (rest.len() - i - 1) as u64 >= size;
            if take(li, &line, &mut out, end) {
                let cut = out.len() >= max && li < l1 && !end;
                out.truncate(out.floor_char_boundary(max));
                return (out, cut);
            }
            line.clear();
            li += 1;
            rest = &rest[i + 1..];
        }
        if line.len() < LINE_CAP {
            line.extend_from_slice(&rest[..rest.len().min(LINE_CAP - line.len())]);
        }
    }
    let cut = out.len() > max;
    out.truncate(out.floor_char_boundary(max));
    (out, cut)
}

/// A line as shown: tabs as spaces, a CR at its end dropped, cut at the cap.
fn line_text(b: &[u8]) -> String {
    let b = b.strip_suffix(b"\r").unwrap_or(b);
    let s = String::from_utf8_lossy(b);
    let s = if b.len() >= LINE_CAP { format!("{}…", &s[..s.floor_char_boundary(LINE_CAP - 4)]) } else { s.into_owned() };
    if s.contains('\t') { s.replace('\t', "    ") } else { s }
}

/// The bytes a hex search looks for: pairs of hex digits ("de ad", "0xdead"), else the
/// text as written.
pub fn hex_query(q: &str) -> Vec<u8> {
    let t = q.trim();
    let digits: String = t.trim_start_matches("0x").chars().filter(|c| !c.is_whitespace()).collect();
    let pairs = (t.starts_with("0x") || t.contains(' ')) && digits.len() >= 2 && digits.len() % 2 == 0 && digits.bytes().all(|b| b.is_ascii_hexdigit());
    if pairs {
        return (0..digits.len()).step_by(2).filter_map(|i| u8::from_str_radix(&digits[i..i + 2], 16).ok()).collect();
    }
    q.as_bytes().to_vec()
}

/// A search through a big file on its own thread; a newer one stops it.
pub struct Find {
    pub query: String,
    pub hits: Arc<Mutex<Vec<u64>>>,
    /// Bytes looked through; `u64::MAX` once finished.
    pub seen: Arc<AtomicU64>,
    /// The hit shown, by its place among the hits.
    pub cur: usize,
    /// The first match was scrolled to.
    pub shown: bool,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Find {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Find {
    fn start(path: &Path, q: &str, hex: bool, ctx: &egui::Context) -> Find {
        let hits = Arc::new(Mutex::new(vec![]));
        let seen = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let needle = if hex { hex_query(q) } else { q.to_ascii_lowercase().into_bytes() };
        let (path, h, s, st, ctx) = (path.to_path_buf(), hits.clone(), seen.clone(), stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            if let Ok(f) = File::open(&path) {
                scan(&f, &needle, !hex, &h, &s, &st, &ctx);
            }
            s.store(u64::MAX, Ordering::Relaxed);
            ctx.request_repaint();
        });
        Find { query: q.to_string(), hits, seen, cur: 0, shown: false, stop }
    }

    pub fn done(&self) -> bool {
        self.seen.load(Ordering::Relaxed) == u64::MAX
    }
}

/// Every place `needle` occurs: as line numbers (one per line) when `lines`, else as
/// byte offsets.
fn scan(f: &File, needle: &[u8], lines: bool, hits: &Mutex<Vec<u64>>, seen: &AtomicU64, stop: &std::sync::atomic::AtomicBool, ctx: &egui::Context) {
    if needle.is_empty() {
        return;
    }
    // The last bytes of a chunk start the next one, so a match across the seam is found.
    let keep = needle.len() - 1;
    let mut buf = vec![0u8; (1 << 20) + keep];
    let (mut at, mut carried, mut line) = (0u64, 0usize, 0u64);
    let mut found = vec![];
    while !stop.load(Ordering::Relaxed) {
        let n = f.read_at(&mut buf[carried..], at).unwrap_or(0);
        if n == 0 {
            break;
        }
        let end = carried + n;
        if lines {
            buf[carried..end].make_ascii_lowercase();
        }
        let (hay, base) = (&buf[..end], at - carried as u64);
        // Matches start before `lim`; line breaks are counted up to it, each once.
        let lim = end.saturating_sub(keep);
        let mut counted = 0;
        for i in 0..lim {
            if hay[i] == needle[0] && hay[i..].starts_with(needle) {
                if !lines {
                    found.push(base + i as u64);
                    continue;
                }
                line += hay[counted..i].iter().filter(|&&b| b == b'\n').count() as u64;
                counted = i;
                if found.last() != Some(&line) {
                    found.push(line);
                }
            }
        }
        line += hay[counted..lim].iter().filter(|&&b| b == b'\n').count() as u64;
        at += n as u64;
        carried = end - lim;
        buf.copy_within(lim..end, 0);
        seen.store(at, Ordering::Relaxed);
        if !found.is_empty() {
            let mut h = hits.lock().unwrap();
            let room = MAX_HITS.saturating_sub(h.len());
            h.extend(found.drain(..).take(room));
            ctx.request_repaint();
            if h.len() >= MAX_HITS {
                return;
            }
        }
    }
}
