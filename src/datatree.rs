//! JSON, JSON Lines, TOML and YAML as a tree the inspector can fold: parsed on a worker
//! into one flat list of nodes in file order (keys keep the order they were written in),
//! drawn one row per visible node.

use egui::{Align2, CornerRadius, FontId, Key, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use crate::theme::{Glyph, Palette};

/// Values kept from one file; past this the tree ends with a note.
pub const MAX_NODES: usize = 200_000;
/// JSON Lines: records read.
pub const JSONL_LINES: usize = 1000;

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Map(Vec<(String, V)>),
    List(Vec<V>),
    Str(String),
    /// Numbers as written: a 20-digit id stays exact.
    Num(String),
    Bool(bool),
    Null,
    /// TOML dates and times, YAML timestamps: shown as written.
    Date(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum K {
    Map,
    List,
    Str,
    Num,
    Bool,
    Null,
    Date,
}

#[derive(Debug)]
pub struct Node {
    /// Key, or the index in a list (`index` set).
    pub key: String,
    pub index: bool,
    pub kind: K,
    /// A scalar as shown; empty for maps and lists.
    pub text: String,
    pub depth: u16,
    pub parent: u32,
    /// One past the last node of this one's subtree.
    pub end: u32,
    /// Children of a map or list.
    pub count: u32,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub open: Vec<bool>,
    /// Visible nodes in order, made again when something folds.
    rows: Vec<u32>,
    rows_ok: bool,
    pub sel: usize,
    /// The tree has the keyboard (a row was clicked).
    pub focus: bool,
    reveal: bool,
    pub note: Option<String>,
    /// Text selected across rows: (node, byte of its row as shown).
    pub tsel: Option<crate::textsel::Sel>,
}

// ---------------------------------------------------------------- JSON

/// Strict enough to reject what is not JSON, lenient where editors are: comments and
/// trailing commas (settings files have both) are accepted.
pub fn parse_json(s: &str) -> Result<V, String> {
    let mut p = Json { b: s.as_bytes(), i: 0, budget: MAX_NODES };
    let v = p.value(0)?;
    p.ws();
    if p.i < p.b.len() && p.budget > 0 {
        return Err(p.err("Text after the value"));
    }
    Ok(v)
}

struct Json<'a> {
    b: &'a [u8],
    i: usize,
    budget: usize,
}

impl Json<'_> {
    fn err(&self, what: &str) -> String {
        let line = self.b[..self.i.min(self.b.len())].iter().filter(|&&c| c == b'\n').count() + 1;
        format!("{what} on line {line}")
    }
    fn ws(&mut self) {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.b[self.i..].starts_with(b"//") {
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
            } else if self.b[self.i..].starts_with(b"/*") {
                match self.b[self.i + 2..].windows(2).position(|w| w == b"*/") {
                    Some(p) => self.i += p + 4,
                    None => self.i = self.b.len(),
                }
            } else {
                return;
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<V, String> {
        self.ws();
        if depth > 512 {
            return Err(self.err("Nested too deeply"));
        }
        self.budget = self.budget.saturating_sub(1);
        let Some(&c) = self.b.get(self.i) else { return Err(self.err("Unexpected end")) };
        match c {
            b'{' => {
                self.i += 1;
                let mut m = vec![];
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(V::Map(m));
                        }
                        Some(b'"') => {}
                        None if self.budget == 0 => return Ok(V::Map(m)),
                        _ => return Err(self.err("Expected a key")),
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(self.err("Expected ':'"));
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    m.push((k, v));
                    if self.budget == 0 {
                        return Ok(V::Map(m));
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return Err(self.err("Expected ',' or '}'")),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut l = vec![];
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Ok(V::List(l));
                    }
                    l.push(self.value(depth + 1)?);
                    if self.budget == 0 {
                        return Ok(V::List(l));
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return Err(self.err("Expected ',' or ']'")),
                    }
                }
            }
            b'"' => self.string().map(V::Str),
            b't' if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(V::Bool(true))
            }
            b'f' if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(V::Bool(false))
            }
            b'n' if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(V::Null)
            }
            b'-' | b'0'..=b'9' => {
                let s = self.i;
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    self.i += 1;
                }
                Ok(V::Num(String::from_utf8_lossy(&self.b[s..self.i]).into_owned()))
            }
            _ => Err(self.err("Unexpected character")),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out: Vec<u8> = vec![];
        loop {
            let Some(&c) = self.b.get(self.i) else { return Err(self.err("Unterminated string")) };
            self.i += 1;
            match c {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else { return Err(self.err("Unterminated string")) };
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = |j: &mut usize| -> Option<u32> {
                                let h = std::str::from_utf8(self.b.get(*j..*j + 4)?).ok()?;
                                *j += 4;
                                u32::from_str_radix(h, 16).ok()
                            };
                            let mut j = self.i;
                            let mut cp = hex(&mut j).ok_or_else(|| self.err("Bad \\u escape"))?;
                            // A surrogate pair is one character.
                            if (0xD800..0xDC00).contains(&cp) && self.b.get(j..j + 2) == Some(b"\\u") {
                                let mut k = j + 2;
                                if let Some(lo) = hex(&mut k).filter(|lo| (0xDC00..0xE000).contains(lo)) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    j = k;
                                }
                            }
                            self.i = j;
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
    }
}

/// JSON Lines: one value per line, the first `JSONL_LINES` of them.
pub fn parse_jsonl(s: &str) -> Result<V, String> {
    let mut l = vec![];
    for (n, line) in s.lines().enumerate() {
        if l.len() >= JSONL_LINES {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        l.push(parse_json(line).map_err(|e| format!("Line {}: {}", n + 1, e.rsplit_once(" on line").map_or(e.as_str(), |x| x.0)))?);
    }
    Ok(V::List(l))
}

// ---------------------------------------------------------------- TOML

/// TOML 1.0: tables and arrays of tables, dotted and quoted keys, inline tables, arrays,
/// every kind of string, numbers in their notations, dates and times.
pub fn parse_toml(s: &str) -> Result<V, String> {
    let mut p = Toml { b: s.as_bytes(), i: 0 };
    let mut root = V::Map(vec![]);
    // The table key/value lines go into: a path of keys from the root (an array of
    // tables' path means its last element).
    let mut cur: Vec<String> = vec![];
    loop {
        p.blank();
        let Some(&c) = p.b.get(p.i) else { break };
        if c == b'[' {
            let array = p.b.get(p.i + 1) == Some(&b'[');
            p.i += if array { 2 } else { 1 };
            p.spaces();
            let path = p.key()?;
            p.spaces();
            let close: &[u8] = if array { b"]]" } else { b"]" };
            if !p.b[p.i..].starts_with(close) {
                return Err(p.err("Expected ']'"));
            }
            p.i += close.len();
            p.end_line()?;
            let parent = walk(&mut root, &path[..path.len() - 1]).ok_or_else(|| p.err("Key is not a table"))?;
            let V::Map(m) = parent else { return Err(p.err("Key is not a table")) };
            let last = &path[path.len() - 1];
            if array {
                match m.iter_mut().find(|(k, _)| k == last) {
                    Some((_, V::List(l))) => l.push(V::Map(vec![])),
                    Some(_) => return Err(p.err("Key is not an array of tables")),
                    None => m.push((last.clone(), V::List(vec![V::Map(vec![])]))),
                }
            } else if !m.iter().any(|(k, _)| k == last) {
                m.push((last.clone(), V::Map(vec![])));
            }
            cur = path;
            continue;
        }
        let path = p.key()?;
        p.spaces();
        if p.b.get(p.i) != Some(&b'=') {
            return Err(p.err("Expected '='"));
        }
        p.i += 1;
        p.spaces();
        let v = p.value(0)?;
        p.end_line()?;
        let mut full = cur.clone();
        full.extend_from_slice(&path[..path.len() - 1]);
        let t = walk(&mut root, &full).ok_or_else(|| p.err("Key is not a table"))?;
        let V::Map(m) = t else { return Err(p.err("Key is not a table")) };
        let last = &path[path.len() - 1];
        if m.iter().any(|(k, _)| k == last) {
            return Err(p.err(&format!("'{last}' is defined twice")));
        }
        m.push((last.clone(), v));
    }
    Ok(root)
}

/// The table at `path` below `v`, made as needed; through an array of tables, its last.
fn walk<'a>(v: &'a mut V, path: &[String]) -> Option<&'a mut V> {
    let mut v = v;
    for k in path {
        let m = match v {
            V::Map(m) => m,
            V::List(l) => match l.last_mut() {
                Some(V::Map(m)) => m,
                _ => return None,
            },
            _ => return None,
        };
        let at = match m.iter().position(|(x, _)| x == k) {
            Some(i) => i,
            None => {
                m.push((k.clone(), V::Map(vec![])));
                m.len() - 1
            }
        };
        v = &mut m[at].1;
        if let V::List(l) = v {
            if !matches!(l.last(), Some(V::Map(_))) {
                return None;
            }
        }
    }
    if let V::List(l) = v {
        return l.last_mut();
    }
    Some(v)
}

struct Toml<'a> {
    b: &'a [u8],
    i: usize,
}

impl Toml<'_> {
    fn err(&self, what: &str) -> String {
        let line = self.b[..self.i.min(self.b.len())].iter().filter(|&&c| c == b'\n').count() + 1;
        format!("{what} on line {line}")
    }
    fn spaces(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }
    /// Whitespace, line breaks and comments.
    fn blank(&mut self) {
        loop {
            while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.i += 1;
            }
            if self.b.get(self.i) == Some(&b'#') {
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
            } else {
                return;
            }
        }
    }
    fn end_line(&mut self) -> Result<(), String> {
        self.spaces();
        if self.b.get(self.i) == Some(&b'#') {
            while self.i < self.b.len() && self.b[self.i] != b'\n' {
                self.i += 1;
            }
        }
        match self.b.get(self.i) {
            None | Some(b'\n') => Ok(()),
            Some(b'\r') if self.b.get(self.i + 1) == Some(&b'\n') => Ok(()),
            _ => Err(self.err("Expected the end of the line")),
        }
    }
    fn key(&mut self) -> Result<Vec<String>, String> {
        let mut path = vec![];
        loop {
            self.spaces();
            let part = match self.b.get(self.i) {
                Some(b'"') => self.basic(false)?,
                Some(b'\'') => self.literal(false)?,
                _ => {
                    let s = self.i;
                    while self.b.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')) {
                        self.i += 1;
                    }
                    if s == self.i {
                        return Err(self.err("Expected a key"));
                    }
                    String::from_utf8_lossy(&self.b[s..self.i]).into_owned()
                }
            };
            path.push(part);
            self.spaces();
            if self.b.get(self.i) == Some(&b'.') {
                self.i += 1;
            } else {
                return Ok(path);
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<V, String> {
        if depth > 128 {
            return Err(self.err("Nested too deeply"));
        }
        let rest = &self.b[self.i..];
        match rest.first() {
            Some(b'"') => self.basic(rest.starts_with(b"\"\"\"")).map(V::Str),
            Some(b'\'') => self.literal(rest.starts_with(b"'''")).map(V::Str),
            Some(b'[') => {
                self.i += 1;
                let mut l = vec![];
                loop {
                    self.blank();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Ok(V::List(l));
                    }
                    l.push(self.value(depth + 1)?);
                    self.blank();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return Err(self.err("Expected ',' or ']'")),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = V::Map(vec![]);
                loop {
                    self.spaces();
                    if self.b.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Ok(m);
                    }
                    let path = self.key()?;
                    self.spaces();
                    if self.b.get(self.i) != Some(&b'=') {
                        return Err(self.err("Expected '='"));
                    }
                    self.i += 1;
                    self.spaces();
                    let v = self.value(depth + 1)?;
                    let Some(V::Map(t)) = walk(&mut m, &path[..path.len() - 1]) else { return Err(self.err("Key is not a table")) };
                    t.push((path[path.len() - 1].clone(), v));
                    self.spaces();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return Err(self.err("Expected ',' or '}'")),
                    }
                }
            }
            _ => {
                // Numbers, booleans, dates: everything up to a delimiter. A date and time
                // may be split by one space ("1979-05-27 07:32:00").
                let s = self.i;
                loop {
                    match self.b.get(self.i) {
                        Some(b',' | b']' | b'}' | b'#' | b'\n' | b'\r' | b'\t') | None => break,
                        Some(b' ') => {
                            let date = self.i - s == 10 && self.b[s + 4] == b'-' && self.b.get(self.i + 1).is_some_and(|c| c.is_ascii_digit());
                            if !date {
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                let w = std::str::from_utf8(&self.b[s..self.i]).unwrap_or("").trim_end();
                match w {
                    "true" => Ok(V::Bool(true)),
                    "false" => Ok(V::Bool(false)),
                    "" => Err(self.err("Expected a value")),
                    _ if w.len() >= 8 && (w.as_bytes()[4] == b'-' || w.as_bytes()[2] == b':') && w.as_bytes()[0].is_ascii_digit() => Ok(V::Date(w.to_string())),
                    _ if is_toml_number(w) => Ok(V::Num(w.to_string())),
                    _ => Err(self.err(&format!("'{w}' is not a value"))),
                }
            }
        }
    }
    /// "…" or """…""" with escapes.
    fn basic(&mut self, multi: bool) -> Result<String, String> {
        self.i += if multi { 3 } else { 1 };
        if multi {
            self.skip_newline();
        }
        let mut out: Vec<u8> = vec![];
        loop {
            let Some(&c) = self.b.get(self.i) else { return Err(self.err("Unterminated string")) };
            if c == b'"' {
                if !multi {
                    self.i += 1;
                    break;
                }
                if self.b[self.i..].starts_with(b"\"\"\"") {
                    // Up to two quotes may sit right before the closing three.
                    let mut n = 3;
                    while self.b.get(self.i + n) == Some(&b'"') && n < 5 {
                        n += 1;
                    }
                    out.extend(std::iter::repeat_n(b'"', n - 3));
                    self.i += n;
                    break;
                }
            }
            if c == b'\n' && !multi {
                return Err(self.err("Unterminated string"));
            }
            self.i += 1;
            if c != b'\\' {
                out.push(c);
                continue;
            }
            let Some(&e) = self.b.get(self.i) else { return Err(self.err("Unterminated string")) };
            self.i += 1;
            match e {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'e' => out.push(27),
                b'"' | b'\\' => out.push(e),
                b'u' | b'U' | b'x' => {
                    let n = match e {
                        b'u' => 4,
                        b'U' => 8,
                        _ => 2,
                    };
                    let cp = self.b.get(self.i..self.i + n).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.err("Bad escape"))?;
                    self.i += n;
                    let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                // A backslash at the end of a line joins it to the next.
                b' ' | b'\t' | b'\r' | b'\n' if multi => {
                    while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                        self.i += 1;
                    }
                }
                _ => return Err(self.err("Bad escape")),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
    /// '…' or '''…''', taken as written.
    fn literal(&mut self, multi: bool) -> Result<String, String> {
        self.i += if multi { 3 } else { 1 };
        if multi {
            self.skip_newline();
        }
        let s = self.i;
        let close: &[u8] = if multi { b"'''" } else { b"'" };
        loop {
            if self.i >= self.b.len() || (!multi && self.b[self.i] == b'\n') {
                return Err(self.err("Unterminated string"));
            }
            if self.b[self.i..].starts_with(close) {
                let mut e = self.i;
                self.i += close.len();
                while multi && self.b.get(self.i) == Some(&b'\'') && self.i - e < 5 {
                    self.i += 1;
                    e += 1;
                }
                return Ok(String::from_utf8_lossy(&self.b[s..e]).into_owned());
            }
            self.i += 1;
        }
    }
    fn skip_newline(&mut self) {
        if self.b[self.i..].starts_with(b"\r\n") {
            self.i += 2;
        } else if self.b.get(self.i) == Some(&b'\n') {
            self.i += 1;
        }
    }
}

fn is_toml_number(w: &str) -> bool {
    let t = w.strip_prefix(['+', '-']).unwrap_or(w);
    if matches!(t, "inf" | "nan") {
        return true;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0o")).or_else(|| t.strip_prefix("0b")) {
        return !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit() || c == b'_');
    }
    t.bytes().next().is_some_and(|c| c.is_ascii_digit()) && t.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'e' | b'E' | b'+' | b'-'))
}

// ---------------------------------------------------------------- YAML

/// YAML 1.2 through saphyr's event parser: anchors are followed (an alias shows what it
/// names), scalars typed by the core schema, several documents become a list of them.
pub fn parse_yaml(s: &str) -> Result<V, String> {
    use saphyr_parser::{Event, Parser, ScalarStyle};
    enum Open {
        Map(Vec<(String, V)>, Option<String>, usize),
        List(Vec<V>, usize),
    }
    let mut stack: Vec<Open> = vec![];
    let mut docs: Vec<V> = vec![];
    let mut anchors: Vec<(usize, V)> = vec![];
    let mut budget = MAX_NODES;
    // Hand a finished value to whatever holds it: a key or value of a map, an item.
    fn place(stack: &mut [Open], docs: &mut Vec<V>, anchors: &mut Vec<(usize, V)>, v: V, anchor: usize) {
        if anchor > 0 {
            anchors.push((anchor, v.clone()));
        }
        match stack.last_mut() {
            Some(Open::Map(m, key, _)) => match key.take() {
                Some(k) => m.push((k, v)),
                None => *key = Some(key_text(&v)),
            },
            Some(Open::List(l, _)) => l.push(v),
            None => docs.push(v),
        }
    }
    for ev in Parser::new_from_str(s) {
        let (ev, _) = ev.map_err(|e| format!("{} on line {}", e.info(), e.marker().line()))?;
        if budget == 0 {
            break;
        }
        budget -= 1;
        match ev {
            Event::Scalar(text, style, anchor, tag) => {
                let tagged = tag.as_ref().map(|t| t.suffix.as_str());
                let v = if style != ScalarStyle::Plain || tagged == Some("str") { V::Str(text.into_owned()) } else { yaml_scalar(&text) };
                place(&mut stack, &mut docs, &mut anchors, v, anchor);
            }
            Event::Alias(id) => {
                let v = anchors.iter().rev().find(|(a, _)| *a == id).map(|(_, v)| v.clone()).unwrap_or(V::Null);
                budget = budget.saturating_sub(count(&v));
                place(&mut stack, &mut docs, &mut anchors, v, 0);
            }
            Event::SequenceStart(anchor, _) => stack.push(Open::List(vec![], anchor)),
            Event::MappingStart(anchor, _) => stack.push(Open::Map(vec![], None, anchor)),
            Event::SequenceEnd | Event::MappingEnd => {
                let (v, anchor) = match stack.pop() {
                    Some(Open::List(l, a)) => (V::List(l), a),
                    Some(Open::Map(m, _, a)) => (V::Map(m), a),
                    None => continue,
                };
                place(&mut stack, &mut docs, &mut anchors, v, anchor);
            }
            _ => {}
        }
    }
    // Cut short by the budget: close what is still open.
    while let Some(o) = stack.pop() {
        let v = match o {
            Open::List(l, _) => V::List(l),
            Open::Map(m, _, _) => V::Map(m),
        };
        place(&mut stack, &mut docs, &mut anchors, v, 0);
    }
    Ok(match docs.len() {
        0 => V::Null,
        1 => docs.pop().unwrap_or(V::Null),
        _ => V::List(docs),
    })
}

fn count(v: &V) -> usize {
    match v {
        V::Map(m) => 1 + m.iter().map(|(_, v)| count(v)).sum::<usize>(),
        V::List(l) => 1 + l.iter().map(count).sum::<usize>(),
        _ => 1,
    }
}

/// A mapping key as text (YAML allows any value as a key).
fn key_text(v: &V) -> String {
    match v {
        V::Str(s) | V::Num(s) | V::Date(s) => s.clone(),
        V::Bool(b) => b.to_string(),
        V::Null => "null".into(),
        V::Map(_) => "{…}".into(),
        V::List(_) => "[…]".into(),
    }
}

/// A plain scalar typed as the YAML 1.2 core schema types it.
fn yaml_scalar(s: &str) -> V {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return V::Null,
        "true" | "True" | "TRUE" => return V::Bool(true),
        "false" | "False" | "FALSE" => return V::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "-.inf" | "-.Inf" | ".nan" | ".NaN" | ".NAN" => return V::Num(s.into()),
        _ => {}
    }
    let b = s.as_bytes();
    let t = s.strip_prefix(['-', '+']).unwrap_or(s);
    let int = !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit());
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0o")).is_some_and(|h| !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit()));
    let float = !int && t.bytes().next().is_some_and(|c| c.is_ascii_digit() || c == b'.') && t.parse::<f64>().is_ok();
    if int || hex || float {
        return V::Num(s.into());
    }
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' && b[..4].iter().all(u8::is_ascii_digit) {
        return V::Date(s.into());
    }
    V::Str(s.into())
}

// ---------------------------------------------------------------- tree

/// What was parsed, flattened in order. A root map or list is not a row itself: its
/// entries are the top level.
pub fn flatten(v: &V, note: Option<String>) -> Tree {
    let mut nodes = vec![];
    fn push(nodes: &mut Vec<Node>, key: String, index: bool, v: &V, depth: u16, parent: u32) {
        let at = nodes.len();
        let (kind, text) = match v {
            V::Map(_) => (K::Map, String::new()),
            V::List(_) => (K::List, String::new()),
            V::Str(s) => (K::Str, quote(s)),
            V::Num(n) => (K::Num, n.clone()),
            V::Bool(b) => (K::Bool, b.to_string()),
            V::Null => (K::Null, "null".into()),
            V::Date(d) => (K::Date, d.clone()),
        };
        nodes.push(Node { key, index, kind, text, depth, parent, end: 0, count: 0 });
        let count = children(nodes, v, depth + 1, at as u32);
        nodes[at].count = count;
        nodes[at].end = nodes.len() as u32;
    }
    fn children(nodes: &mut Vec<Node>, v: &V, depth: u16, parent: u32) -> u32 {
        match v {
            V::Map(m) => {
                for (k, x) in m {
                    push(nodes, k.clone(), false, x, depth, parent);
                }
                m.len() as u32
            }
            V::List(l) => {
                for (i, x) in l.iter().enumerate() {
                    push(nodes, i.to_string(), true, x, depth, parent);
                }
                l.len() as u32
            }
            _ => 0,
        }
    }
    match v {
        V::Map(_) | V::List(_) => {
            children(&mut nodes, v, 0, u32::MAX);
        }
        _ => push(&mut nodes, String::new(), true, v, 0, u32::MAX),
    }
    // Small files open whole; big ones show their first two levels.
    let all = nodes.len() <= 400;
    let open = nodes.iter().map(|n| all || n.depth < 1).collect();
    let mut t = Tree { nodes, open, rows: vec![], rows_ok: false, sel: 0, focus: false, reveal: false, note, tsel: None };
    t.rebuild();
    t
}

/// A string as a value: in quotes, its line breaks and tabs shown as escapes.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(400) + 2);
    out.push('"');
    for c in s.chars().take(400) {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    if s.chars().nth(400).is_some() {
        out.push('…');
    }
    out.push('"');
    out
}

impl Tree {
    fn rebuild(&mut self) {
        self.rows.clear();
        let mut i = 0;
        while i < self.nodes.len() {
            self.rows.push(i as u32);
            let n = &self.nodes[i];
            i = if matches!(n.kind, K::Map | K::List) && !self.open[i] { n.end as usize } else { i + 1 };
        }
        self.rows_ok = true;
    }

    pub fn rows(&mut self) -> &[u32] {
        if !self.rows_ok {
            self.rebuild();
        }
        &self.rows
    }

    pub fn is_container(&self, i: usize) -> bool {
        self.nodes.get(i).is_some_and(|n| matches!(n.kind, K::Map | K::List) && n.count > 0)
    }

    pub fn toggle(&mut self, i: usize) {
        if self.is_container(i) {
            self.open[i] = !self.open[i];
            self.rows_ok = false;
        }
    }

    /// Where the selected value sits: `.a.b[0]["odd key"]`.
    pub fn path(&self, i: usize) -> String {
        let mut parts = vec![];
        let mut at = i as u32;
        while let Some(n) = self.nodes.get(at as usize) {
            parts.push(if n.index {
                format!("[{}]", n.key)
            } else if !n.key.is_empty() && n.key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') && !n.key.as_bytes()[0].is_ascii_digit() {
                format!(".{}", n.key)
            } else {
                format!("[{}]", quote(&n.key))
            });
            at = n.parent;
        }
        parts.reverse();
        parts.concat()
    }

    /// The keys the tree takes while it has the keyboard.
    pub fn takes(&self, k: Key) -> bool {
        self.focus && matches!(k, Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End | Key::PageUp | Key::PageDown | Key::Enter | Key::Escape)
    }

    pub fn key(&mut self, k: Key) {
        let rows: Vec<u32> = self.rows().to_vec();
        let pos = rows.iter().position(|&r| r as usize == self.sel).unwrap_or(0);
        let go = |d: isize| rows.get(pos.saturating_add_signed(d).min(rows.len().saturating_sub(1))).map_or(0, |&r| r as usize);
        match k {
            Key::ArrowDown => self.sel = go(1),
            Key::ArrowUp => self.sel = go(-1),
            Key::PageDown => self.sel = go(20),
            Key::PageUp => self.sel = go(-20),
            Key::Home => self.sel = go(-(pos as isize)),
            Key::End => self.sel = go(rows.len() as isize),
            Key::Enter => self.toggle(self.sel),
            Key::Escape => self.focus = false,
            // Right opens, then steps into; left closes, then steps out.
            Key::ArrowRight if self.is_container(self.sel) => {
                if self.open[self.sel] {
                    self.sel += 1;
                } else {
                    self.toggle(self.sel);
                }
            }
            Key::ArrowLeft => {
                if self.is_container(self.sel) && self.open[self.sel] {
                    self.toggle(self.sel);
                } else if let Some(n) = self.nodes.get(self.sel).filter(|n| n.parent != u32::MAX) {
                    self.sel = n.parent as usize;
                }
            }
            _ => {}
        }
        self.reveal = true;
    }
}

/// What a row shows, piece by piece, for its colours.
#[derive(Clone, Copy)]
enum Part {
    Key,
    Count,
    Value(K),
}

/// A row's text in pieces: the key, then the value or what a folded map or list holds.
fn parts(n: &Node, open: bool) -> Vec<(std::borrow::Cow<'_, str>, Part)> {
    use std::borrow::Cow;
    let mut v = vec![];
    if !n.key.is_empty() || !n.index {
        let k = if n.index { Cow::Borrowed(n.key.as_str()) } else if n.key.is_empty() || n.key.contains([' ', ':', '"']) { Cow::Owned(quote(&n.key)) } else { Cow::Borrowed(n.key.as_str()) };
        v.push((k, Part::Key));
        v.push((Cow::Borrowed(if matches!(n.kind, K::Map | K::List) { "" } else { ": " }), Part::Key));
    }
    match n.kind {
        K::Map | K::List => {
            let (o, c) = if n.kind == K::Map { ("{", "}") } else { ("[", "]") };
            if n.count == 0 {
                v.push((Cow::Owned(format!("  {o}{c}")), Part::Key));
            } else if !open {
                v.push((Cow::Owned(format!("  {o}…{c} ")), Part::Key));
                v.push((Cow::Owned(count_text(n)), Part::Count));
            }
        }
        k => v.push((Cow::Borrowed(n.text.as_str()), Part::Value(k))),
    }
    v
}

/// A string value as shown, without its quotes. Its escapes stay: the shown form does
/// not mark a backslash that was there, so `\n` could be either.
fn unquote(s: &str) -> String {
    s.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(s).to_string()
}

impl Tree {
    /// Row `ni`'s text as drawn, and where its value starts (a scalar's).
    pub fn row_text(&self, ni: usize) -> (String, Option<usize>) {
        let Some(n) = self.nodes.get(ni) else { return (String::new(), None) };
        let mut s = String::new();
        let mut value = None;
        for (t, part) in parts(n, self.open[ni]) {
            if matches!(part, Part::Value(_)) {
                value = Some(s.len());
            }
            s.push_str(&t);
        }
        (s, value)
    }

    /// What Copy takes: the text selected across rows (each row after the first indented
    /// as it is drawn), else the value of the row picked: a string without its quotes, a
    /// map or list as its rows.
    pub fn copy_text(&self) -> String {
        let rows = &self.rows;
        let sel = match self.tsel.filter(|s| !s.is_empty()) {
            Some(s) => s,
            None if self.focus => match self.nodes.get(self.sel) {
                Some(n) if !matches!(n.kind, K::Map | K::List) => return if n.kind == K::Str { unquote(&n.text) } else { n.text.clone() },
                Some(n) => crate::textsel::Sel::new((self.sel, 0), ((n.end as usize).saturating_sub(1), usize::MAX)),
                None => return String::new(),
            },
            None => return String::new(),
        };
        let ((n0, _), (n1, _)) = sel.span();
        let start = rows.partition_point(|&r| (r as usize) < n0);
        let base = rows.get(start).and_then(|&r| self.nodes.get(r as usize)).map_or(0, |n| n.depth);
        let mut out = String::new();
        let mut k = start;
        while let Some(&r) = rows.get(k).filter(|&&r| (r as usize) <= n1) {
            let ni = r as usize;
            let (text, _) = self.row_text(ni);
            if let Some((a, b, nl)) = sel.on_line(ni, text.len()) {
                if a == 0 && k > start {
                    out.push_str(&"  ".repeat(self.nodes[ni].depth.saturating_sub(base) as usize));
                }
                out.push_str(text.get(a..b).unwrap_or(""));
                if nl && rows.get(k + 1).is_some_and(|&x| (x as usize) <= n1) {
                    out.push('\n');
                }
            }
            k += 1;
        }
        out
    }
}

fn count_text(n: &Node) -> String {
    match (n.kind, n.count) {
        (K::Map, 1) => "1 key".into(),
        (K::Map, c) => format!("{} keys", crate::table::fmt_count(c as usize)),
        (K::List, 1) => "1 item".into(),
        (K::List, c) => format!("{} items", crate::table::fmt_count(c as usize)),
        _ => String::new(),
    }
}

/// Draw the tree; `copied` gets the path when its button is pressed.
pub fn view(ui: &mut Ui, body: Rect, t: &mut Tree, size: f32, pal: &Palette, spacing: f32, id: u64) -> Option<String> {
    if t.nodes.is_empty() {
        ui.painter().text(body.center(), Align2::CENTER_CENTER, "Empty", FontId::proportional(12.5), pal.text_dim);
        return None;
    }
    let font = FontId::monospace(size);
    let row_h = crate::inspector::row_height(ui, size, spacing);
    let lift = ((row_h - ui.fonts_mut(|f| f.row_height(&font))) / 2.0).floor().max(0.0);
    let indent = (size * 1.3).round();
    // The path bar along the bottom: where the selected value sits, and a copy button.
    let bar_h = (row_h + 8.0).max(26.0);
    let list = Rect::from_min_max(body.min, pos2(body.right(), body.bottom() - bar_h));
    let bar = Rect::from_min_max(pos2(body.left(), list.bottom()), body.max);
    let mut copied = None;
    let reveal = std::mem::take(&mut t.reveal);
    let n_rows = t.rows().len() + usize::from(t.note.is_some());
    let mut inner = ui.new_child(UiBuilder::new().max_rect(list.shrink2(vec2(4.0, 4.0))));
    egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("datatree", id)).show_viewport(&mut inner, |ui, vp| {
        let cw = ui.fonts_mut(|f| f.glyph_width(&font, 'M'));
        let widest = t.rows.iter().take(3000).map(|&r| {
            let n = &t.nodes[r as usize];
            n.depth as f32 * indent + (n.key.chars().count() + n.text.chars().count().min(200) + 14) as f32 * cw
        }).fold(0.0, f32::max);
        let (r, resp) = ui.allocate_exact_size(vec2((widest + 24.0).max(vp.width()), n_rows as f32 * row_h + 4.0), Sense::click_and_drag());
        // Text across rows: the place under a point is a node and a byte of its row.
        let text_x = |ni: usize| r.left() + 4.0 + t.nodes[ni].depth as f32 * indent + indent + 2.0;
        if !t.rows.is_empty() {
            let rows = &t.rows;
            let tree = &*t;
            let hit = |pp: egui::Pos2| -> crate::textsel::At {
                let y = pp.y - r.top();
                if y < 0.0 {
                    return (rows[0] as usize, 0);
                }
                let i = (y / row_h) as usize;
                let Some(&ni) = rows.get(i) else {
                    let last = rows[rows.len() - 1] as usize;
                    return (last, tree.row_text(last).0.len());
                };
                let ni = ni as usize;
                (ni, crate::textsel::byte_at_col(&tree.row_text(ni).0, (pp.x - text_x(ni)) / cw.max(1.0)))
            };
            // A double-click on a value takes all of it (a string without its quotes).
            let word = |(ni, b): crate::textsel::At| {
                let (text, value) = tree.row_text(ni);
                let (x, y) = match value.filter(|&v| b >= v) {
                    Some(v) if tree.nodes[ni].kind == K::Str && text.len() >= v + 2 => (v + 1, text.len() - 1),
                    Some(v) => (v, text.len()),
                    None => crate::textsel::word(&text, b),
                };
                ((ni, x), (ni, y))
            };
            let line = |(ni, _): crate::textsel::At| {
                let k = rows.partition_point(|&x| (x as usize) < ni);
                match rows.get(k + 1) {
                    Some(&next) => ((ni, 0), (next as usize, 0)),
                    None => ((ni, 0), (ni, tree.row_text(ni).0.len())),
                }
            };
            let mut ts = t.tsel;
            crate::textsel::track(ui, &resp, &mut ts, hit, word, line);
            t.tsel = ts;
        }
        let tsel = t.tsel.filter(|s| !s.is_empty());
        let first = (vp.min.y / row_h).floor().max(0.0) as usize;
        let last = ((vp.max.y / row_h).ceil() as usize + 1).min(n_rows);
        let p = ui.painter();
        let hover = resp.hover_pos();
        let mut clicked: Option<(usize, bool)> = None;
        for i in first..last {
            let y = r.top() + i as f32 * row_h;
            let Some(&ni) = t.rows.get(i) else {
                if let Some(note) = &t.note {
                    p.text(pos2(r.left() + 6.0, y + lift), Align2::LEFT_TOP, note, font.clone(), pal.text_dim);
                }
                continue;
            };
            let ni = ni as usize;
            let n = &t.nodes[ni];
            let row = Rect::from_min_max(pos2(r.left() + vp.min.x, y), pos2(r.left() + vp.max.x, y + row_h));
            // While text is selected the row picked shows as it does without the
            // keyboard, so the selection reads over it.
            let lit = t.focus && tsel.is_none();
            if ni == t.sel {
                p.rect_filled(row, CornerRadius::same(3), if lit { pal.selected } else { pal.hover });
            } else if hover.is_some_and(|h| row.contains(h)) {
                p.rect_filled(row, CornerRadius::same(3), pal.hover.gamma_multiply(0.6));
            }
            let on_sel = ni == t.sel && lit;
            let dim = if on_sel { pal.on_sel_dim() } else { pal.text_dim };
            let x0 = r.left() + 4.0 + n.depth as f32 * indent;
            let chev = Rect::from_min_size(pos2(x0, y), vec2(indent, row_h));
            let container = matches!(n.kind, K::Map | K::List) && n.count > 0;
            if container {
                let g = if t.open[ni] { Glyph::ChevronDown } else { Glyph::ChevronRight };
                crate::theme::paint_glyph(p, Rect::from_center_size(chev.center(), egui::Vec2::splat((size * 0.8).round())), g, dim);
            }
            let mut job = egui::text::LayoutJob::default();
            let fmt = |c| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
            let s = &pal.syntax;
            let value_col = |k: K| match k {
                K::Str => s.string,
                K::Num => s.number,
                K::Bool | K::Null => s.keyword,
                K::Date => s.kind,
                _ => pal.text,
            };
            for (text, part) in parts(n, t.open[ni]) {
                let c = match part {
                    Part::Key => dim,
                    Part::Count => dim.gamma_multiply(0.8),
                    Part::Value(k) => if on_sel { pal.on_sel() } else { value_col(k) },
                };
                job.append(&text, 0.0, fmt(c));
            }
            if let Some((a, b, nl)) = tsel.and_then(|s| s.on_line(ni, job.text.len())) {
                let x0 = chev.right() + 2.0 + crate::textsel::cols(&job.text, 0, a) * cw;
                let x1 = chev.right() + 2.0 + crate::textsel::cols(&job.text, 0, b) * cw + if nl { cw * 0.6 } else { 0.0 };
                if x1 > x0 {
                    crate::textsel::paint(p, Rect::from_min_max(pos2(x0, y), pos2(x1, y + row_h)), pal);
                }
            }
            let g = ui.fonts_mut(|f| f.layout_job(job));
            p.galley(pos2(chev.right() + 2.0, y + lift), g, pal.text);
            if let Some(h) = hover.filter(|h| row.contains(*h)) {
                if resp.clicked() {
                    clicked = Some((ni, container && h.x < chev.right() + 2.0));
                }
                if resp.double_clicked() && container {
                    clicked = Some((ni, true));
                }
            }
            if reveal && ni == t.sel {
                ui.scroll_to_rect(row, None);
            }
        }
        if let Some((ni, fold)) = clicked {
            t.sel = ni;
            t.focus = true;
            if fold {
                t.toggle(ni);
            }
        }
    });
    let _ = t.rows();
    // Path bar.
    let p = ui.painter();
    p.line_segment([bar.left_top(), bar.right_top()], Stroke::new(1.0, pal.border));
    let copy = Rect::from_center_size(pos2(bar.right() - 18.0, bar.center().y), vec2(24.0, 20.0));
    let path = t.path(t.sel);
    let g = crate::listing::elided_tail(ui, &path, FontId::monospace((size - 1.0).max(8.0)), (copy.left() - bar.left() - 16.0).max(8.0), pal.text_dim);
    p.galley(pos2(bar.left() + 8.0, bar.center().y - g.size().y / 2.0), g, pal.text_dim);
    if bar.width() > 80.0 {
        let resp = ui.interact(copy, egui::Id::new(("copy-path", id)), Sense::click());
        crate::ui::note_control(ui, copy, "copy path");
        if resp.hovered() {
            p.rect_filled(copy, CornerRadius::same(4), pal.hover);
        }
        crate::theme::paint_glyph(p, Rect::from_center_size(copy.center(), egui::Vec2::splat(13.0)), Glyph::Copy, pal.text_dim);
        if resp.on_hover_text("Copy path").clicked() {
            ui.ctx().copy_text(path.clone());
            copied = Some(path);
        }
    }
    // A click anywhere else gives the keyboard back to the listing.
    if ui.input(|i| i.pointer.any_pressed()) && ui.ctx().pointer_interact_pos().is_some_and(|pp| !body.contains(pp)) {
        t.focus = false;
    }
    copied
}
