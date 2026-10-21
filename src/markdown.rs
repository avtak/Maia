//! Markdown read into blocks and inline runs on a worker, then laid out at the preview's
//! width and drawn: headings, paragraphs, emphasis, code, quotes, lists with tasks,
//! tables, rules, links and local pictures. The common CommonMark and GitHub forms are
//! covered; anything else stays text, never lost.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontId, Galley, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2};

use crate::theme::Palette;

pub const STRONG: u8 = 1;
pub const EM: u8 = 2;
pub const CODE: u8 = 4;
pub const STRIKE: u8 = 8;
/// A formula: its TeX in `text`, its number (from 1) in `link`.
pub const MATH: u8 = 16;

/// Source rendered at most; past it the preview says the rest is in Source.
pub const MAX_BYTES: usize = 512 << 10;

/// Text with one style; `link` and `img` are 1-based indexes (0: none).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Run {
    pub text: String,
    pub st: u8,
    pub link: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Run>),
    Para(Vec<Run>),
    Code { lang: String, lines: Vec<String>, spans: Option<Vec<Vec<(usize, usize, crate::syntax::Kind)>>> },
    Quote(Vec<Block>),
    /// `start`: the first number of an ordered list. Items: task state and content.
    List { start: Option<u64>, items: Vec<(Option<bool>, Vec<Block>)> },
    Table { aligns: Vec<Align>, head: Vec<Vec<Run>>, rows: Vec<Vec<Vec<Run>>> },
    Rule,
    /// Pictures standing alone (1-based image indexes), each perhaps a link.
    Images(Vec<(u32, u32)>),
    /// A formula on lines of its own (its number, from 1).
    Math(u32),
}

#[derive(Debug, Default, Clone)]
pub struct Link {
    pub url: String,
    /// A file or folder the link names, resolved against the document's folder.
    pub path: Option<PathBuf>,
    pub dir: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Image {
    pub src: String,
    pub alt: String,
    /// The picture on disk when the source names one that exists.
    pub path: Option<(PathBuf, Option<std::time::SystemTime>)>,
}

#[derive(Debug, Default)]
pub struct Doc {
    pub blocks: Vec<Block>,
    pub links: Vec<Link>,
    pub images: Vec<Image>,
    /// Formulas: TeX, and whether each is shown on a line of its own.
    pub maths: Vec<(String, bool)>,
    /// Only the first `MAX_BYTES` were rendered.
    pub cut: bool,
}

struct Cx {
    refs: HashMap<String, String>,
    links: Vec<Link>,
    images: Vec<Image>,
    maths: Vec<(String, bool)>,
}

// ---------------------------------------------------------------- blocks

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn blank(l: &str) -> bool {
    l.trim().is_empty()
}

/// An opening code fence: its character, length and info string.
fn fence(l: &str) -> Option<(u8, usize, &str)> {
    if indent(l) > 3 {
        return None;
    }
    let t = l.trim_start();
    let c = *t.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = t.bytes().take_while(|&b| b == c).count();
    let info = t[n..].trim();
    (n >= 3 && !(c == b'`' && info.contains('`'))).then_some((c, n, info))
}

fn rule(l: &str) -> bool {
    if indent(l) > 3 {
        return false;
    }
    let t: Vec<u8> = l.bytes().filter(|b| *b != b' ' && *b != b'\t').collect();
    t.len() >= 3 && matches!(t[0], b'*' | b'-' | b'_') && t.iter().all(|&b| b == t[0])
}

fn atx(l: &str) -> Option<(u8, &str)> {
    if indent(l) > 3 {
        return None;
    }
    let t = l.trim_start();
    let n = t.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&n) || !(t.len() == n || t.as_bytes()[n] == b' ') {
        return None;
    }
    let mut s = t[n..].trim();
    // A closing run of #s goes, when a space sets it apart.
    let stripped = s.trim_end_matches('#');
    if stripped.is_empty() || stripped.ends_with(' ') {
        s = stripped.trim_end();
    }
    Some((n as u8, s))
}

fn quote(l: &str) -> Option<&str> {
    if indent(l) > 3 {
        return None;
    }
    let t = l.trim_start().strip_prefix('>')?;
    Some(t.strip_prefix(' ').unwrap_or(t))
}

/// A list item's marker: (number for an ordered item, the marker's own character,
/// where the item's content starts).
fn marker(l: &str) -> Option<(Option<u64>, u8, usize)> {
    let ind = indent(l);
    if ind > 3 {
        return None;
    }
    let t = &l[ind..];
    let b = t.as_bytes();
    let (num, ch, len) = match b.first()? {
        b'-' | b'*' | b'+' => (None, b[0], 1),
        b'0'..=b'9' => {
            let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 9 || !matches!(b.get(d), Some(b'.' | b')')) {
                return None;
            }
            (t[..d].parse().ok(), b[d], d + 1)
        }
        _ => return None,
    };
    match b.get(len) {
        None => Some((num, ch, ind + len)),
        Some(b' ') => {
            let sp = b[len..].iter().take_while(|&&c| c == b' ').count();
            // Five or more spaces start indented code inside the item: only one counts.
            Some((num, ch, ind + len + if sp >= 5 || len + sp >= b.len() { 1 } else { sp }))
        }
        _ => None,
    }
}

fn starts_block(l: &str) -> bool {
    fence(l).is_some() || atx(l).is_some() || rule(l) || quote(l).is_some() || marker(l).is_some_and(|m| m.2 < l.len())
}

fn delim_row(l: &str) -> Option<Vec<Align>> {
    let cells = split_row(l);
    if cells.is_empty() || !l.contains('-') {
        return None;
    }
    cells
        .iter()
        .map(|c| {
            let c = c.trim();
            let (l, r) = (c.starts_with(':'), c.ends_with(':'));
            let mid = c.trim_matches(':');
            (!mid.is_empty() && mid.bytes().all(|b| b == b'-')).then_some(match (l, r) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// Cells of a table row: split at bars that are not escaped or inside code.
fn split_row(l: &str) -> Vec<&str> {
    let t = l.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") { &t[..t.len() - 1] } else { t };
    let mut out = vec![];
    let (mut start, mut code, mut esc) = (0, false, false);
    for (i, c) in t.char_indices() {
        match c {
            '\\' if !esc => {
                esc = true;
                continue;
            }
            '`' => code = !code,
            '|' if !esc && !code => {
                out.push(&t[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        esc = false;
    }
    out.push(&t[start..]);
    out
}

/// `[label]: destination "title"`.
fn ref_def(l: &str) -> Option<(String, String)> {
    if indent(l) > 3 {
        return None;
    }
    let t = l.trim_start().strip_prefix('[')?;
    let close = t.find("]:")?;
    let label = &t[..close];
    if label.trim().is_empty() || label.contains(['[', ']']) {
        return None;
    }
    let rest = t[close + 2..].trim();
    let dest = if let Some(r) = rest.strip_prefix('<') { r.split('>').next()? } else { rest.split_whitespace().next()? };
    Some((norm_label(label), dest.to_string()))
}

fn norm_label(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The source read into a document. `base`: the document's folder, for links and
/// pictures named by relative paths (looked up on disk: workers only).
pub fn parse(lines: &[String], base: Option<&Path>) -> Doc {
    let mut total = 0;
    let mut cut = false;
    let mut src: Vec<&str> = Vec::with_capacity(lines.len());
    for l in lines {
        total += l.len() + 1;
        if total > MAX_BYTES {
            cut = true;
            break;
        }
        src.push(l.as_str());
    }
    let mut refs = HashMap::new();
    let mut in_fence = false;
    for l in &src {
        if fence(l).is_some() {
            in_fence = !in_fence;
        } else if !in_fence {
            if let Some((k, v)) = ref_def(l) {
                refs.entry(k).or_insert(v);
            }
        }
    }
    let mut cx = Cx { refs, links: vec![], images: vec![], maths: vec![] };
    let blocks = blocks(&src, &mut cx, 0);
    if let Some(base) = base {
        for l in &mut cx.links {
            if let Some(p) = local(&l.url, base) {
                if let Ok(m) = std::fs::metadata(&p) {
                    l.dir = m.is_dir();
                    l.path = Some(p);
                }
            }
        }
        for im in &mut cx.images {
            if let Some(p) = local(&im.src, base) {
                if let Some(m) = std::fs::metadata(&p).ok().filter(|m| m.is_file()) {
                    im.path = Some((p, m.modified().ok()));
                }
            }
        }
    }
    Doc { blocks, links: cx.links, images: cx.images, maths: cx.maths, cut }
}

/// A relative link or picture source as a path below `base` (no scheme, no anchor).
fn local(url: &str, base: &Path) -> Option<PathBuf> {
    if url.is_empty() || url.starts_with('#') || url.contains("://") || url.starts_with("mailto:") || url.starts_with("data:") {
        return None;
    }
    let u = url.split(['#', '?']).next()?;
    let p = Path::new(&*percent_decode(u)).to_path_buf();
    Some(if p.is_absolute() { p } else { base.join(p) })
}

pub(crate) fn percent_decode(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('%') {
        return s.into();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned().into()
}

fn blocks(lines: &[&str], cx: &mut Cx, depth: usize) -> Vec<Block> {
    let mut out = vec![];
    let mut para: Vec<&str> = vec![];
    let mut i = 0;
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Block>, cx: &mut Cx| {
        // Reference definitions are not text.
        let keep: Vec<&str> = para.iter().copied().skip_while(|l| ref_def(l).is_some()).collect();
        para.clear();
        if keep.is_empty() {
            return;
        }
        let text = keep.join("\n");
        let runs = inlines(text.trim_end(), cx, 0);
        out.push(match only_images(&runs, cx) {
            Some(v) => Block::Images(v),
            None => Block::Para(runs),
        });
    };
    if depth > 24 {
        // Quotes in quotes in lists, without end: the rest is text.
        para.extend_from_slice(lines);
        flush(&mut para, &mut out, cx);
        return out;
    }
    while i < lines.len() {
        let l = lines[i];
        if blank(l) {
            flush(&mut para, &mut out, cx);
            i += 1;
            continue;
        }
        let ind = indent(l);
        if ind >= 4 && para.is_empty() {
            let mut code = vec![];
            while i < lines.len() && (blank(lines[i]) || indent(lines[i]) >= 4) {
                code.push(lines[i].get(4..).unwrap_or("").to_string());
                i += 1;
            }
            while code.last().is_some_and(|c| c.trim().is_empty()) {
                code.pop();
            }
            out.push(Block::Code { lang: String::new(), lines: code, spans: None });
            continue;
        }
        if para.is_empty() && ind <= 3 {
            if let Some((tex, next)) = crate::texmath::block_at(lines, i) {
                cx.maths.push((tex, true));
                out.push(Block::Math(cx.maths.len() as u32));
                i = next;
                continue;
            }
        }
        if let Some((c, n, info)) = fence(l) {
            flush(&mut para, &mut out, cx);
            let lang = info.split_whitespace().next().unwrap_or("").to_string();
            let mut code = vec![];
            i += 1;
            while i < lines.len() {
                let x = lines[i];
                let t = x.trim();
                if indent(x) <= 3 && t.len() >= n && t.bytes().all(|b| b == c) {
                    i += 1;
                    break;
                }
                // The fence's own indent comes off each line.
                let strip = indent(x).min(ind);
                code.push(x[strip..].to_string());
                i += 1;
            }
            // GitHub's other way of writing a formula block.
            if lang.eq_ignore_ascii_case("math") {
                cx.maths.push((code.join("\n").trim().to_string(), true));
                out.push(Block::Math(cx.maths.len() as u32));
                continue;
            }
            let spans = crate::syntax::lang_for(&format!("x.{}", fence_ext(&lang))).map(|lg| crate::syntax::highlight(lg, &code, 5000));
            out.push(Block::Code { lang, lines: code, spans });
            continue;
        }
        if let Some((lvl, text)) = atx(l) {
            flush(&mut para, &mut out, cx);
            out.push(Block::Heading(lvl, inlines(text, cx, 0)));
            i += 1;
            continue;
        }
        if !para.is_empty() && ind <= 3 {
            let t = l.trim();
            let lvl = if t.bytes().all(|b| b == b'=') { 1 } else if t.bytes().all(|b| b == b'-') { 2 } else { 0 };
            if lvl > 0 {
                let text = para.join("\n");
                para.clear();
                out.push(Block::Heading(lvl, inlines(text.trim(), cx, 0)));
                i += 1;
                continue;
            }
        }
        if rule(l) {
            flush(&mut para, &mut out, cx);
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if quote(l).is_some() {
            flush(&mut para, &mut out, cx);
            let mut inner: Vec<&str> = vec![];
            while i < lines.len() {
                match quote(lines[i]) {
                    Some(r) => inner.push(r),
                    // A paragraph inside goes on lazily, without its '>'.
                    None if !blank(lines[i]) && !starts_block(lines[i]) && inner.last().is_some_and(|p| !blank(p)) => inner.push(lines[i]),
                    None => break,
                }
                i += 1;
            }
            out.push(Block::Quote(blocks(&inner, cx, depth + 1)));
            continue;
        }
        if let Some(m) = marker(l) {
            // Inside a paragraph only a bullet or a "1." with text starts a list.
            let interrupts = m.2 < l.len() && (m.0.is_none() || m.0 == Some(1));
            if para.is_empty() || interrupts {
                flush(&mut para, &mut out, cx);
                out.push(list(lines, &mut i, cx, depth));
                continue;
            }
        }
        if para.is_empty() && l.contains('|') {
            if let Some(aligns) = lines.get(i + 1).and_then(|n| delim_row(n)) {
                let head = split_row(l);
                if head.len() == aligns.len() {
                    let cells = |row: &str, cx: &mut Cx| -> Vec<Vec<Run>> {
                        let mut v: Vec<Vec<Run>> = split_row(row).iter().take(aligns.len()).map(|c| inlines(c.trim(), cx, 0)).collect();
                        v.resize(aligns.len(), vec![]);
                        v
                    };
                    let head = cells(l, cx);
                    i += 2;
                    let mut rows = vec![];
                    while i < lines.len() && !blank(lines[i]) && !starts_block(lines[i]) {
                        rows.push(cells(lines[i], cx));
                        i += 1;
                    }
                    out.push(Block::Table { aligns, head, rows });
                    continue;
                }
            }
        }
        if para.is_empty() && ind <= 3 && is_html_start(l.trim_start()) {
            let mut html = vec![];
            while i < lines.len() && !blank(lines[i]) {
                html.push(lines[i]);
                i += 1;
            }
            out.extend(html_block(&html.join("\n"), cx));
            continue;
        }
        para.push(l.trim_start());
        i += 1;
    }
    flush(&mut para, &mut out, cx);
    out
}

/// A fence's language as the extension the highlighter knows it by.
fn fence_ext(lang: &str) -> String {
    let l = lang.to_ascii_lowercase();
    match l.as_str() {
        "rust" => "rs",
        "python" | "python3" => "py",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "bash" | "shell" | "console" | "shellsession" | "zsh" => "sh",
        "c++" => "cpp",
        "golang" => "go",
        "kotlin" => "kt",
        "csharp" | "c#" => "cs",
        "ruby" => "rb",
        "yml" => "yaml",
        "make" | "makefile" => "mk",
        _ => return l,
    }
    .into()
}

fn list(lines: &[&str], i: &mut usize, cx: &mut Cx, depth: usize) -> Block {
    let Some((start, ch, _)) = marker(lines[*i]) else { return Block::Rule };
    let mut items = vec![];
    while *i < lines.len() {
        let l = lines[*i];
        let Some((num, c, col)) = marker(l).filter(|m| m.1 == ch && m.0.is_some() == start.is_some()) else { break };
        let _ = num;
        let _ = c;
        let mut item: Vec<&str> = vec![l.get(col..).unwrap_or("")];
        *i += 1;
        let mut last_blank = false;
        while *i < lines.len() {
            let x = lines[*i];
            if blank(x) {
                item.push("");
                last_blank = true;
                *i += 1;
                continue;
            }
            if indent(x) >= col {
                item.push(&x[col..]);
            } else if !last_blank && !starts_block(x) {
                item.push(x.trim_start());
            } else {
                break;
            }
            last_blank = false;
            *i += 1;
        }
        while item.last().is_some_and(|x| x.is_empty()) {
            item.pop();
        }
        // "[ ]" and "[x]" make a task.
        let mut task = None;
        if let Some(first) = item.first_mut() {
            for (p, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(rest) = first.strip_prefix(p) {
                    task = Some(done);
                    *first = rest;
                }
            }
        }
        items.push((task, blocks(&item, cx, depth + 1)));
        // A blank line, then something that is not the next item: the list is over.
        if *i < lines.len() && marker(lines[*i]).is_none() {
            break;
        }
    }
    Block::List { start, items }
}

fn is_html_start(t: &str) -> bool {
    let b = t.as_bytes();
    b.first() == Some(&b'<') && b.get(1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'/' || *c == b'!')
}

/// HTML as GitHub READMEs use it: pictures (inside links or not) and text. Tags go,
/// what they hold stays.
fn html_block(html: &str, cx: &mut Cx) -> Vec<Block> {
    let mut out = vec![];
    let mut runs: Vec<Run> = vec![];
    let mut imgs: Vec<(u32, u32)> = vec![];
    let mut link = 0u32;
    let mut text = String::new();
    let flush_text = |text: &mut String, runs: &mut Vec<Run>, link: u32| {
        let t = decode_entities(text);
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            if runs.last().is_some_and(|r| r.link == link && !r.text.ends_with('\n')) {
                runs.push(Run { text: " ".into(), st: 0, link: 0 });
            }
            runs.push(Run { text: t, st: 0, link });
        }
        text.clear();
    };
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").map_or("", |e| &r[e + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else {
            text.push_str(rest);
            rest = "";
            break;
        };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        flush_text(&mut text, &mut runs, link);
        let attr = |key: &str| -> Option<String> {
            let lower = tag.to_ascii_lowercase();
            let at = lower.find(&format!("{key}="))? + key.len() + 1;
            let v = &tag[at..];
            let (q, v) = match v.as_bytes().first() {
                Some(b'"') => ('"', &v[1..]),
                Some(b'\'') => ('\'', &v[1..]),
                _ => (' ', v),
            };
            Some(decode_entities(v.split(q).next().unwrap_or("")).into_owned())
        };
        match name.as_str() {
            "a" if !tag.starts_with('/') => {
                link = attr("href").map_or(0, |h| add_link(cx, h));
            }
            "a" => link = 0,
            "img" => {
                if let Some(src) = attr("src") {
                    cx.images.push(Image { src, alt: attr("alt").unwrap_or_default(), path: None });
                    imgs.push((cx.images.len() as u32, link));
                }
            }
            "br" | "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr" => {
                if runs.last().is_some_and(|r| !r.text.ends_with('\n')) {
                    runs.push(Run { text: "\n".into(), st: 0, link: 0 });
                }
            }
            _ => {}
        }
        if !imgs.is_empty() && runs.iter().any(|r| !r.text.trim().is_empty()) {
            // Text and pictures in their order.
            push_runs(&mut out, &mut runs);
            out.push(Block::Images(std::mem::take(&mut imgs)));
        }
    }
    text.push_str(rest);
    flush_text(&mut text, &mut runs, link);
    if !imgs.is_empty() {
        push_runs(&mut out, &mut runs);
        out.push(Block::Images(imgs));
    }
    push_runs(&mut out, &mut runs);
    out
}

fn push_runs(out: &mut Vec<Block>, runs: &mut Vec<Run>) {
    while runs.last().is_some_and(|r| r.text.trim().is_empty()) {
        runs.pop();
    }
    while runs.first().is_some_and(|r| r.text.trim().is_empty()) {
        runs.remove(0);
    }
    if !runs.is_empty() {
        out.push(Block::Para(std::mem::take(runs)));
    }
}

/// A paragraph of nothing but pictures (each perhaps a link) stands as pictures.
fn only_images(runs: &[Run], cx: &Cx) -> Option<Vec<(u32, u32)>> {
    let mut v = vec![];
    for r in runs {
        match img_index(r) {
            Some(i) if (i as usize) <= cx.images.len() => v.push((i, r.link)),
            _ if r.text.trim().is_empty() => {}
            _ => return None,
        }
    }
    (!v.is_empty()).then_some(v)
}

/// Pictures inline are runs whose text is a private marker followed by their index.
const IMG: char = '\u{F8FF}';

fn img_index(r: &Run) -> Option<u32> {
    r.text.strip_prefix(IMG)?.parse().ok().filter(|&i| i > 0)
}

fn add_link(cx: &mut Cx, url: String) -> u32 {
    cx.links.push(Link { url, path: None, dir: false });
    cx.links.len() as u32
}

// ---------------------------------------------------------------- inlines

enum Tok {
    Text(Run),
    Delim { ch: u8, n: usize, open: bool, close: bool, st: u8 },
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace())
}

pub(crate) fn decode_entities(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('&') {
        return s.into();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('&') {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        let end = rest[1..].find(';').map(|e| e + 1).filter(|&e| e <= 10);
        let ch = end.and_then(|e| {
            let name = &rest[1..e];
            Some(match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => '\u{a0}',
                "copy" => '©',
                "mdash" => '—',
                "ndash" => '–',
                "hellip" => '…',
                _ => {
                    let n = name.strip_prefix('#')?;
                    let v = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(v).filter(|c| *c != '\0')?
                }
            })
        });
        match (ch, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out.into()
}

/// The closing bracket for the `[` at `open`, skipping nested brackets, escapes and code.
fn close_bracket(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0usize, open);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'`' => {
                let n = b[i..].iter().take_while(|&&c| c == b'`').count();
                let tick = "`".repeat(n);
                if let Some(e) = s[i + n..].find(&tick) {
                    i += n + e + n;
                    continue;
                }
                i += n;
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `(destination "title")` after a link's text: the destination and where it ends.
fn destination(s: &str, at: usize) -> Option<(String, usize)> {
    let rest = s.get(at..)?.strip_prefix('(')?;
    let t = rest.trim_start_matches([' ', '\n']);
    let skip = rest.len() - t.len();
    let (dest, mut used) = if let Some(r) = t.strip_prefix('<') {
        let e = r.find('>')?;
        (r[..e].to_string(), e + 2)
    } else {
        let mut depth = 0i32;
        let mut e = t.len();
        for (i, c) in t.char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    e = i;
                    break;
                }
                ')' => depth -= 1,
                ' ' | '\n' => {
                    e = i;
                    break;
                }
                _ => {}
            }
        }
        (t[..e].replace('\\', ""), e)
    };
    // A title, then the closing parenthesis.
    let after = &t[used..];
    let a2 = after.trim_start_matches([' ', '\n']);
    used += after.len() - a2.len();
    let mut a3 = a2;
    if let Some(q) = a2.chars().next().filter(|c| matches!(c, '"' | '\'' | '(')) {
        let close = if q == '(' { ')' } else { q };
        let e = a2[1..].find(close)? + 2;
        used += e;
        a3 = &a2[e..];
        let a4 = a3.trim_start_matches([' ', '\n']);
        used += a3.len() - a4.len();
        a3 = a4;
    }
    a3.starts_with(')').then_some((dest, at + 1 + skip + used + 1))
}

fn inlines(s: &str, cx: &mut Cx, depth: usize) -> Vec<Run> {
    let mut toks: Vec<Tok> = vec![];
    let mut buf = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    let flush = |buf: &mut String, toks: &mut Vec<Tok>| {
        if !buf.is_empty() {
            toks.push(Tok::Text(Run { text: decode_entities(buf).into_owned(), st: 0, link: 0 }));
            buf.clear();
        }
    };
    let prev_char = |s: &str, i: usize| s[..i].chars().next_back().unwrap_or(' ');
    while i < b.len() {
        let c = b[i];
        match c {
            b'$' | b'\\' if (c == b'$' || b.get(i + 1) == Some(&b'(')) && crate::texmath::inline_at(s, i).is_some() => {
                let Some((end, tex, display)) = crate::texmath::inline_at(s, i) else { break };
                flush(&mut buf, &mut toks);
                cx.maths.push((tex.clone(), display));
                toks.push(Tok::Text(Run { text: tex, st: MATH, link: cx.maths.len() as u32 }));
                i = end;
            }
            b'\\' if b.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                buf.push(b[i + 1] as char);
                i += 2;
            }
            b'\\' if b.get(i + 1) == Some(&b'\n') => {
                buf.push('\n');
                i += 2;
            }
            b'`' => {
                let n = b[i..].iter().take_while(|&&c| c == b'`').count();
                let tick = "`".repeat(n);
                let mut j = i + n;
                let mut found = None;
                while let Some(e) = s[j..].find(&tick) {
                    let at = j + e;
                    let run = b[at..].iter().take_while(|&&c| c == b'`').count();
                    if run == n {
                        found = Some(at);
                        break;
                    }
                    j = at + run;
                }
                match found {
                    Some(e) => {
                        flush(&mut buf, &mut toks);
                        let mut code = s[i + n..e].replace('\n', " ");
                        if code.len() > 1 && code.starts_with(' ') && code.ends_with(' ') && !code.trim().is_empty() {
                            code = code[1..code.len() - 1].to_string();
                        }
                        toks.push(Tok::Text(Run { text: code, st: CODE, link: 0 }));
                        i = e + n;
                    }
                    None => {
                        buf.push_str(&tick);
                        i += n;
                    }
                }
            }
            b'!' if b.get(i + 1) == Some(&b'[') && depth < 8 => {
                match link_at(s, i + 1, cx) {
                    Some((inner, url, end)) => {
                        flush(&mut buf, &mut toks);
                        let alt: String = inlines(inner, cx, depth + 1).iter().filter(|r| img_index(r).is_none()).map(|r| r.text.as_str()).collect();
                        cx.images.push(Image { src: url, alt, path: None });
                        toks.push(Tok::Text(Run { text: format!("{IMG}{}", cx.images.len()), st: 0, link: 0 }));
                        i = end;
                    }
                    None => {
                        buf.push('!');
                        i += 1;
                    }
                }
            }
            b'[' if depth < 8 => match link_at(s, i, cx) {
                Some((inner, url, end)) => {
                    flush(&mut buf, &mut toks);
                    let id = add_link(cx, url);
                    for mut r in inlines(inner, cx, depth + 1) {
                        if r.link == 0 {
                            r.link = id;
                        }
                        toks.push(Tok::Text(r));
                    }
                    i = end;
                }
                None => {
                    buf.push('[');
                    i += 1;
                }
            },
            b'<' => {
                // <https://…> and <someone@example.com>.
                let auto = s[i + 1..].find('>').map(|e| &s[i + 1..i + 1 + e]).filter(|u| !u.contains([' ', '<', '\n']) && (u.contains("://") || u.starts_with("mailto:") || (u.contains('@') && !u.contains('/'))));
                match auto {
                    Some(u) => {
                        flush(&mut buf, &mut toks);
                        let url = if u.contains(':') { u.to_string() } else { format!("mailto:{u}") };
                        let id = add_link(cx, url);
                        toks.push(Tok::Text(Run { text: u.to_string(), st: 0, link: id }));
                        i += u.len() + 2;
                    }
                    None => {
                        buf.push('<');
                        i += 1;
                    }
                }
            }
            b'h' | b'w' if (s[i..].starts_with("https://") || s[i..].starts_with("http://") || s[i..].starts_with("www.")) && !prev_char(s, i).is_alphanumeric() => {
                // A bare address, less the punctuation that ends a sentence after it.
                let mut e = s[i..].find(|c: char| c.is_whitespace() || c == '<').map_or(s.len(), |e| i + e);
                while e > i && matches!(b[e - 1], b'.' | b',' | b':' | b';' | b'!' | b'?' | b'*' | b'_' | b'~' | b'\'' | b'"') {
                    e -= 1;
                }
                if b[e - 1] == b')' && s[i..e].matches('(').count() < s[i..e].matches(')').count() {
                    e -= 1;
                }
                let u = &s[i..e];
                flush(&mut buf, &mut toks);
                let url = if u.starts_with("www.") { format!("https://{u}") } else { u.to_string() };
                let id = add_link(cx, url);
                toks.push(Tok::Text(Run { text: u.to_string(), st: 0, link: id }));
                i = e;
            }
            b'*' | b'_' | b'~' => {
                let n = b[i..].iter().take_while(|&&x| x == c).count();
                let before = prev_char(s, i);
                let after = s[i + n..].chars().next().unwrap_or(' ');
                let left = !after.is_whitespace() && (!is_punct(after) || before.is_whitespace() || is_punct(before));
                let right = !before.is_whitespace() && (!is_punct(before) || after.is_whitespace() || is_punct(after));
                let (open, close) = match c {
                    b'_' => (left && (!right || is_punct(before)), right && (!left || is_punct(after))),
                    b'~' if n > 2 => (false, false),
                    _ => (left, right),
                };
                flush(&mut buf, &mut toks);
                toks.push(Tok::Delim { ch: c, n, open, close, st: 0 });
                i += n;
            }
            b'\n' => {
                // Two spaces before a line break keep it; any other break is a space.
                if buf.ends_with("  ") {
                    let t = buf.trim_end_matches(' ').len();
                    buf.truncate(t);
                    buf.push('\n');
                } else {
                    let t = buf.trim_end_matches(' ').len();
                    buf.truncate(t);
                    buf.push(' ');
                }
                i += 1;
            }
            _ => {
                let ch = s[i..].chars().next().unwrap_or(' ');
                buf.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    flush(&mut buf, &mut toks);
    emphasis(&mut toks);
    let mut out: Vec<Run> = vec![];
    for t in toks {
        let r = match t {
            Tok::Text(r) => r,
            Tok::Delim { ch, n, st, .. } if n > 0 => Run { text: (ch as char).to_string().repeat(n), st, link: 0 },
            _ => continue,
        };
        match out.last_mut() {
            Some(l) if l.st == r.st && l.link == r.link && img_index(l).is_none() && img_index(&r).is_none() => l.text.push_str(&r.text),
            _ => out.push(r),
        }
    }
    out
}

/// A link (or a picture's brackets) at `open`: its text, its destination (inline or by
/// reference) and where it ends.
fn link_at<'a>(s: &'a str, open: usize, cx: &Cx) -> Option<(&'a str, String, usize)> {
    let close = close_bracket(s, open)?;
    let inner = &s[open + 1..close];
    if let Some((url, end)) = destination(s, close + 1) {
        return Some((inner, url, end));
    }
    // [text][label], [text][] and [label].
    if s[close + 1..].starts_with('[') {
        let e = s[close + 2..].find(']')? + close + 2;
        let label = &s[close + 2..e];
        let key = norm_label(if label.is_empty() { inner } else { label });
        return cx.refs.get(&key).map(|u| (inner, u.clone(), e + 1));
    }
    cx.refs.get(&norm_label(inner)).map(|u| (inner, u.clone(), close + 1))
}

/// Pair `*`, `_` and `~` runs into emphasis, strong emphasis and strikethrough, closers
/// from the left each with the nearest opener that fits, as CommonMark does.
fn emphasis(toks: &mut [Tok]) {
    let mut c = 0;
    while c < toks.len() {
        let (cch, cn, copen) = match toks[c] {
            Tok::Delim { ch, n, close, open, .. } if close && n > 0 => (ch, n, open),
            _ => {
                c += 1;
                continue;
            }
        };
        let mut found = None;
        for o in (0..c).rev() {
            if let Tok::Delim { ch, n, open, close, .. } = toks[o] {
                if ch == cch && open && n > 0 {
                    // The rule of three: a run that both opens and closes pairs only when
                    // the lengths do not add up to a multiple of three.
                    if ch != b'~' && (close || copen) && (n + cn) % 3 == 0 && !(n % 3 == 0 && cn % 3 == 0) {
                        continue;
                    }
                    if ch == b'~' && n != cn {
                        continue;
                    }
                    found = Some((o, n));
                    break;
                }
            }
        }
        let Some((o, on)) = found else {
            c += 1;
            continue;
        };
        let used = if cch == b'~' { cn } else if on >= 2 && cn >= 2 { 2 } else { 1 };
        let style = match (cch, used) {
            (b'~', _) => STRIKE,
            (_, 2) => STRONG,
            _ => EM,
        };
        for t in &mut toks[o + 1..c] {
            match t {
                Tok::Text(r) => r.st |= style,
                Tok::Delim { st, open, close, .. } => {
                    *st |= style;
                    // Runs left open inside a pair stay as text.
                    *open = false;
                    *close = false;
                }
            }
        }
        if let Tok::Delim { n, .. } = &mut toks[o] {
            *n -= used;
        }
        if let Tok::Delim { n, .. } = &mut toks[c] {
            *n -= used;
            if *n == 0 {
                c += 1;
            }
        }
    }
}

/// The anchor GitHub gives a heading: lower case, punctuation gone, spaces as dashes.
pub fn slug(runs: &[Run]) -> String {
    let text: String = runs.iter().map(|r| r.text.as_str()).collect();
    text.trim().to_lowercase().chars().filter_map(|c| if c == ' ' { Some('-') } else if c.is_alphanumeric() || c == '-' || c == '_' { Some(c) } else { None }).collect()
}

/// Every run's text.
#[cfg(test)]
pub fn plain(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

// ---------------------------------------------------------------- layout

enum Piece {
    /// A galley; `bold` the faux-bold pass over its strong parts; links by byte range.
    Text { pos: Pos2, g: Arc<Galley>, bold: Option<Arc<Galley>>, links: Vec<(std::ops::Range<usize>, u32)> },
    Fill { r: Rect, col: Color32, round: u8, line: Option<Color32> },
    Line { a: Pos2, b: Pos2, col: Color32 },
    Dot { c: Pos2, r: f32, col: Color32, hollow: bool },
    Check { r: Rect, done: bool },
    Picture { r: Rect, img: u32, link: u32 },
    /// A typeset formula, tinted in the text's colour.
    Math { r: Rect, tex: egui::TextureId },
}

impl Piece {
    fn rect(&self) -> Rect {
        match self {
            Piece::Text { pos, g, .. } => Rect::from_min_size(*pos, g.size()),
            Piece::Fill { r, .. } | Piece::Check { r, .. } | Piece::Picture { r, .. } | Piece::Math { r, .. } => *r,
            Piece::Line { a, b, .. } => Rect::from_two_pos(*a, *b).expand(1.0),
            Piece::Dot { c, r, .. } => Rect::from_center_size(*c, Vec2::splat(r * 2.0)),
        }
    }
}

pub struct View {
    pub doc: Doc,
    laid: Option<Laid>,
    math: Maths,
    /// A `#heading` link was followed: where to scroll.
    jump: Option<f32>,
    /// Where the document's top left was drawn last.
    origin: Pos2,
    /// Text selected: (text piece, counted in reading order; character in it).
    pub sel: Option<crate::textsel::Sel>,
}

/// The document's formulas, typeset on a thread the first time it is shown.
#[derive(Default)]
struct Maths {
    started: bool,
    rx: Option<std::sync::mpsc::Receiver<Vec<Result<crate::texmath::Typeset, String>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Each formula's picture (its size and depth in pixels), or why it shows as written.
    got: Vec<Result<(egui::TextureHandle, Vec2, f32), String>>,
    /// Why none are typeset here.
    note: Option<String>,
}

impl Drop for Maths {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Maths {
    fn pump(&mut self, ctx: &egui::Context, maths: &[(String, bool)]) {
        if !self.started && !maths.is_empty() {
            self.started = true;
            let tools = crate::drives::Tools::current();
            match tools.as_ref().and_then(crate::texmath::engine) {
                None => self.note = Some(tools.as_ref().map_or("Formulas show as written: install TeX Live to typeset them", crate::texmath::missing).to_string()),
                Some(e) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let (items, stop, c) = (maths.to_vec(), self.stop.clone(), ctx.clone());
                    let t = tools.expect("checked above");
                    let started = std::thread::Builder::new().name("texmath".into()).spawn(move || {
                        let r = crate::texmath::typeset(&t, e, &items, &|| stop.load(std::sync::atomic::Ordering::Relaxed));
                        let _ = tx.send(r);
                        c.request_repaint();
                    });
                    if started.is_ok() {
                        self.rx = Some(rx);
                    }
                }
            }
        }
        if let Some(r) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.rx = None;
            self.got = r
                .into_iter()
                .map(|x| {
                    x.map(|t| {
                        let img = egui::ColorImage::from_rgba_unmultiplied([t.img.w as usize, t.img.h as usize], &t.img.px);
                        (ctx.load_texture("formula", img, egui::TextureOptions::LINEAR), vec2(t.img.w as f32, t.img.h as f32), t.depth)
                    })
                })
                .collect();
            // Say why when TeX itself is what failed, not one formula.
            if self.got.iter().all(|g| g.is_err()) {
                if let Some(Err(w)) = self.got.first() {
                    if w.starts_with("Formulas show as written") {
                        self.note = Some(w.clone());
                    }
                }
            }
        }
    }
}

/// A formula as the layout uses it: its picture's size and depth in points.
#[derive(Clone, Copy)]
struct MathPic {
    tex: egui::TextureId,
    size: Vec2,
    depth: f32,
}

struct Laid {
    key: (u32, u32, u32, [u8; 4], [u8; 4], u32),
    pieces: Vec<Piece>,
    height: f32,
    anchors: Vec<(String, f32)>,
    /// The next block to lay out; one past the end once all are.
    next: usize,
    /// How full egui's glyph atlas was. It only fills, until egui clears it and starts
    /// over; then these galleys point at glyphs that are gone and must be laid out anew.
    atlas: f32,
}

impl View {
    /// Formulas typeset so far, and the note shown when none can be.
    #[cfg(test)]
    pub fn maths_state(&self) -> (usize, Option<&str>) {
        (self.math.got.iter().filter(|g| g.is_ok()).count(), self.math.note.as_deref())
    }

    pub fn new(doc: Doc) -> View {
        View { doc, laid: None, math: Maths::default(), jump: None, origin: Pos2::ZERO, sel: None }
    }

    /// The text selected, as plain text: pieces side by side (table cells) apart by a
    /// tab, a list's number and its item by a space, the rest by line breaks.
    pub fn copy_text(&self) -> String {
        let (Some(sel), Some(l)) = (self.sel.filter(|s| !s.is_empty()), &self.laid) else { return String::new() };
        let ((k0, c0), (k1, c1)) = sel.span();
        let mut out = String::new();
        let mut prev: Option<(f32, &str)> = None;
        for (k, (pos, g)) in texts(&l.pieces).enumerate().skip(k0) {
            if k > k1 {
                break;
            }
            let t = g.text();
            let n = t.chars().count();
            let (a, b) = (if k == k0 { c0.min(n) } else { 0 }, if k == k1 { c1.min(n) } else { n });
            if let Some((y, pt)) = prev {
                let number = pt.strip_suffix('.').is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()));
                out.push(if (pos.y - y).abs() > 0.5 { '\n' } else if number { ' ' } else { '\t' });
            }
            out.push_str(&t[byte_of(t, a)..byte_of(t, b)]);
            prev = Some((pos.y, t));
        }
        out
    }

    /// The selection's ends for tests: (piece, character), in order.
    #[cfg(test)]
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        self.sel.filter(|s| !s.is_empty()).map(|s| s.span())
    }

    /// Where the `ch`th character of the text piece holding `needle` was drawn, for a
    /// simulated press.
    #[cfg(test)]
    pub fn char_pos(&self, needle: &str, ch: usize) -> Option<Pos2> {
        let l = self.laid.as_ref()?;
        let (pos, g) = texts(&l.pieces).find(|(_, g)| g.text().contains(needle))?;
        let ci = g.text()[..g.text().find(needle)?].chars().count() + ch;
        Some(self.origin + pos.to_vec2() + g.pos_from_cursor(egui::text::CCursor::new(ci)).center().to_vec2())
    }
    /// Where a link to `url` was drawn last, for a simulated click.
    #[cfg(test)]
    pub fn link_pos(&self, url: &str) -> Option<Pos2> {
        let l = self.laid.as_ref()?;
        let id = self.doc.links.iter().position(|x| x.url == url)? as u32 + 1;
        l.pieces.iter().find_map(|p| match p {
            Piece::Text { pos, g, links, .. } => links.iter().find(|(_, k)| *k == id).map(|(rg, _)| {
                let ci = g.text()[..rg.start].chars().count() + 1;
                self.origin + pos.to_vec2() + g.pos_from_cursor(egui::text::CCursor::new(ci)).center().to_vec2()
            }),
            Piece::Picture { r, link, .. } if *link == id => Some(self.origin + r.center().to_vec2()),
            _ => None,
        })
    }
    /// Pieces drawn per kind, for tests: (texts, pictures, checkboxes, rules and lines).
    #[cfg(test)]
    pub fn counts(&self) -> Option<(usize, usize, usize, usize)> {
        let l = self.laid.as_ref()?;
        let n = |f: fn(&Piece) -> bool| l.pieces.iter().filter(|p| f(p)).count();
        Some((n(|p| matches!(p, Piece::Text { .. })), n(|p| matches!(p, Piece::Picture { .. })), n(|p| matches!(p, Piece::Check { .. })), n(|p| matches!(p, Piece::Line { .. }))))
    }
}

struct Style<'a> {
    size: f32,
    spacing: f32,
    pal: &'a Palette,
}

struct Writer<'a, 'u> {
    ui: &'u Ui,
    st: Style<'a>,
    pieces: Vec<Piece>,
    anchors: Vec<(String, f32)>,
    y: f32,
    /// Pictures' sizes in points, once loaded (index 0 is image 1).
    pics: &'a [Option<Vec2>],
    images: &'a [Image],
    /// Each formula, once typeset, and its TeX.
    maths: &'a [Option<MathPic>],
    sources: &'a [(String, bool)],
    /// Lists this one is inside (bullets alternate filled and hollow by it).
    lists: usize,
}

impl MathPic {
    /// Shrunk to stay inside a line of text: at most three ems tall and `w` wide.
    fn inline(self, size: f32, w: f32) -> MathPic {
        let k = (size * 3.0 / self.size.y).min(w.max(16.0) / self.size.x).min(1.0);
        MathPic { size: self.size * k, depth: self.depth * k, ..self }
    }
}

impl Writer<'_, '_> {
    fn line_h(&self, size: f32) -> f32 {
        self.ui.fonts_mut(|f| f.row_height(&FontId::proportional(size))) * self.st.spacing.clamp(1.0, 2.0)
    }

    fn job(&self, runs: &[Run], size: f32, color: Color32, w: f32, all_bold: bool, faux: bool) -> (LayoutJob, Vec<(std::ops::Range<usize>, u32)>, bool) {
        let (job, links, bold, _) = self.job_math(runs, size, color, w, all_bold, faux, false);
        (job, links, bold)
    }

    /// `job`, and with `place` a typeset formula keeps its room in the line as a
    /// no-break space after a gap its width: (the space's character index, formula).
    #[allow(clippy::too_many_arguments)]
    fn job_math(&self, runs: &[Run], size: f32, color: Color32, w: f32, all_bold: bool, faux: bool, place: bool) -> (LayoutJob, Vec<(std::ops::Range<usize>, u32)>, bool, Vec<(usize, u32)>) {
        let pal = self.st.pal;
        let mut job = LayoutJob::default();
        job.wrap.max_width = w.max(16.0);
        let lh = self.line_h(size);
        let mut links = vec![];
        let mut maths = vec![];
        let mut any_bold = all_bold;
        for r in runs {
            if r.st & MATH != 0 {
                let pic = self.maths.get(r.link as usize - 1).copied().flatten().map(|m| m.inline(size, w));
                match pic.filter(|_| place) {
                    Some(m) => {
                        // The formula covers its space too, so punctuation after it sits against it.
                        let font = FontId::proportional(size);
                        let sp = self.ui.fonts_mut(|f| f.glyph_width(&font, '\u{a0}'));
                        let fmt = TextFormat { font_id: font, color: Color32::TRANSPARENT, line_height: Some(lh.max(m.size.y + 2.0)), ..Default::default() };
                        maths.push((job.text.chars().count(), r.link));
                        job.append("\u{a0}", (m.size.x - sp).max(0.0), fmt);
                    }
                    None => {
                        let col = if faux { Color32::TRANSPARENT } else { pal.text_dim };
                        let mut fmt = TextFormat { font_id: FontId::monospace((size * 0.9).round()), color: col, line_height: Some(lh), ..Default::default() };
                        if !faux {
                            fmt.background = pal.raised.gamma_multiply(if pal.dark { 0.9 } else { 0.7 });
                        }
                        job.append(&format!("${}$", r.text), 0.0, fmt);
                    }
                }
                continue;
            }
            let bold = all_bold || r.st & STRONG != 0;
            any_bold |= bold;
            let code = r.st & CODE != 0;
            let col = if r.link > 0 { pal.accent } else { color };
            let font = if code { FontId::monospace((size * 0.9).round()) } else { FontId::proportional(size) };
            let mut fmt = TextFormat { font_id: font, color: col, line_height: Some(lh), italics: r.st & EM != 0, ..Default::default() };
            if code && !faux {
                fmt.background = pal.raised.gamma_multiply(if pal.dark { 0.9 } else { 0.7 });
            }
            if r.st & STRIKE != 0 && !faux {
                fmt.strikethrough = Stroke::new(1.0, col);
            }
            if r.link > 0 && !faux {
                fmt.underline = Stroke::new(1.0, pal.accent.gamma_multiply(0.45));
            }
            let text = match img_index(r) {
                // A picture inside a sentence shows its description.
                Some(i) => {
                    fmt.color = pal.text_dim;
                    fmt.italics = true;
                    self.images.get(i as usize - 1).map(|im| if im.alt.is_empty() { "picture".to_string() } else { im.alt.clone() }).unwrap_or_default()
                }
                None => r.text.clone(),
            };
            if faux && !bold {
                fmt.color = Color32::TRANSPARENT;
            }
            let start = job.text.len();
            job.append(&text, 0.0, fmt);
            if r.link > 0 {
                links.push((start..job.text.len(), r.link));
            }
        }
        (job, links, any_bold, maths)
    }

    /// Lay out a run of text at (x, y) in width `w`; returns its height.
    fn text(&mut self, runs: &[Run], x: f32, w: f32, size: f32, color: Color32, all_bold: bool) -> f32 {
        if runs.is_empty() {
            return 0.0;
        }
        let (job, links, any_bold, maths) = self.job_math(runs, size, color, w, all_bold, false, true);
        let g = self.ui.fonts_mut(|f| f.layout_job(job));
        let bold = any_bold.then(|| {
            let (job, ..) = self.job_math(runs, size, color, w, all_bold, true, true);
            self.ui.fonts_mut(|f| f.layout_job(job))
        });
        let h = g.size().y;
        // Each formula fills the gap before its space, standing on the line's baseline.
        if !maths.is_empty() {
            let mut at = 0;
            for row in &g.rows {
                for gl in &row.row.glyphs {
                    if let Some(&(_, k)) = maths.iter().find(|m| m.0 == at) {
                        if let Some(m) = self.maths.get(k as usize - 1).copied().flatten().map(|m| m.inline(size, w)) {
                            let right = x + row.pos.x + gl.pos.x + gl.advance_width;
                            let base = self.y + row.pos.y + gl.pos.y;
                            let r = Rect::from_min_max(pos2(right - m.size.x, base + m.depth - m.size.y), pos2(right, base + m.depth));
                            self.pieces.push(Piece::Math { r, tex: m.tex });
                        }
                    }
                    at += 1;
                }
                if row.row.ends_with_newline {
                    at += 1;
                }
            }
        }
        self.pieces.push(Piece::Text { pos: pos2(x, self.y), g, bold, links });
        h
    }

    fn gap(&self) -> f32 {
        (self.st.size * 0.75).round()
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, w: f32, color: Color32, tight: bool) {
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                self.y += if tight { (self.st.size * 0.3).round() } else { self.gap() };
            }
            self.block(b, x, w, color, tight);
        }
    }

    fn block(&mut self, b: &Block, x: f32, w: f32, color: Color32, tight: bool) {
        let pal = self.st.pal;
        let size = self.st.size;
        match b {
            Block::Math(k) => match self.maths.get(*k as usize - 1).copied().flatten() {
                // Centred on its own line; one wider than the column shrinks to fit.
                Some(m) => {
                    let s = (w / m.size.x).min(1.0);
                    let sz = m.size * s;
                    let pad = (size * 0.3).round();
                    let r = Rect::from_min_size(pos2(x + (w - sz.x) / 2.0, self.y + pad), sz);
                    self.pieces.push(Piece::Math { r, tex: m.tex });
                    self.y += sz.y + pad * 2.0;
                }
                None => {
                    let tex = self.sources.get(*k as usize - 1).map(|m| m.0.clone()).unwrap_or_default();
                    let run = Run { text: format!("$$ {tex} $$"), st: CODE, link: 0 };
                    let h = self.text(std::slice::from_ref(&run), x, w, size, pal.text_dim, false);
                    self.y += h;
                }
            },
            Block::Heading(level, runs) => {
                let k = [1.6, 1.32, 1.15, 1.02, 0.95, 0.9][(*level as usize - 1).min(5)];
                if self.y > 0.0 && *level <= 2 {
                    self.y += (size * 0.5).round();
                }
                self.anchors.push((slug(runs), self.y));
                let col = if *level == 6 { pal.text_dim } else { pal.text };
                let h = self.text(runs, x, w, (size * k).round(), col, true);
                self.y += h;
                if *level <= 2 {
                    self.y += 4.0;
                    self.pieces.push(Piece::Line { a: pos2(x, self.y), b: pos2(x + w, self.y), col: pal.card_line() });
                    self.y += 2.0;
                }
            }
            Block::Para(runs) => {
                let h = self.text(runs, x, w, size, color, false);
                self.y += h;
            }
            Block::Code { lines, spans, .. } => {
                let pad = 8.0;
                let font = FontId::monospace((size * 0.92).round());
                let lh = self.ui.fonts_mut(|f| f.row_height(&font)) * self.st.spacing.clamp(1.0, 2.0);
                let mut job = LayoutJob::default();
                job.wrap.max_width = (w - pad * 2.0).max(16.0);
                // At the edge: the last space that fits is often the end of a line's
                // indent, which would leave a blank row and the code at the left.
                job.wrap.break_anywhere = true;
                let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, line_height: Some(lh), ..Default::default() };
                for (li, line) in lines.iter().enumerate() {
                    let mut at = 0;
                    for &(a, b, k) in spans.as_ref().and_then(|s| s.get(li)).map_or(&[][..], |s| &s[..]) {
                        if a < at || b > line.len() {
                            continue;
                        }
                        if a > at {
                            job.append(&line[at..a], 0.0, fmt(pal.text));
                        }
                        job.append(&line[a..b], 0.0, fmt(crate::inspector::syntax_color(k, pal)));
                        at = b;
                    }
                    job.append(&line[at..], 0.0, fmt(pal.text));
                    if li + 1 < lines.len() {
                        job.append("\n", 0.0, fmt(pal.text));
                    }
                }
                let g = self.ui.fonts_mut(|f| f.layout_job(job));
                let r = Rect::from_min_size(pos2(x, self.y), vec2(w, g.size().y.max(lh) + pad * 2.0));
                self.pieces.push(Piece::Fill { r, col: pal.card(), round: 6, line: Some(pal.card_line()) });
                self.pieces.push(Piece::Text { pos: pos2(x + pad, self.y + pad), g, bold: None, links: vec![] });
                self.y = r.bottom();
            }
            Block::Quote(inner) => {
                let top = self.y;
                let ind = (size * 1.1).round();
                self.blocks(inner, x + ind, w - ind, pal.text_dim, tight);
                self.pieces.push(Piece::Fill { r: Rect::from_min_max(pos2(x + 1.0, top), pos2(x + 4.0, self.y.max(top + 4.0))), col: pal.accent.gamma_multiply(0.45), round: 2, line: None });
            }
            Block::List { start, items } => {
                let digits = start.map_or(1, |s| (s + items.len() as u64).to_string().len());
                let ind = if start.is_some() { (size * (0.9 + 0.6 * digits as f32)).round() } else { (size * 1.5).round() };
                let first_h = self.line_h(size);
                for (n, (task, blocks)) in items.iter().enumerate() {
                    if n > 0 {
                        self.y += (size * 0.3).round();
                    }
                    let cy = self.y + first_h / 2.0;
                    let mut cx = x + ind;
                    match (task, start) {
                        (Some(done), _) => {
                            let s = (size * 0.85).round();
                            self.pieces.push(Piece::Check { r: Rect::from_center_size(pos2(x + ind - s * 0.9, cy), Vec2::splat(s)), done: *done });
                        }
                        (None, Some(s0)) => {
                            // Laid out as the item's text is, so the two share a baseline.
                            let run = Run { text: format!("{}.", s0 + n as u64), st: 0, link: 0 };
                            let (job, ..) = self.job(std::slice::from_ref(&run), size, pal.text_dim, f32::INFINITY, false, false);
                            let g = self.ui.fonts_mut(|f| f.layout_job(job));
                            let pos = pos2(x + ind - 6.0 - g.size().x, self.y);
                            self.pieces.push(Piece::Text { pos, g, bold: None, links: vec![] });
                        }
                        (None, None) => {
                            let r = (size * 0.17).max(2.0);
                            self.pieces.push(Piece::Dot { c: pos2(x + ind * 0.45, cy), r, col: pal.text_dim, hollow: self.lists % 2 == 1 });
                        }
                    }
                    if task.is_some() {
                        cx = x + ind + (size * 0.2).round();
                    }
                    let before = self.y;
                    self.lists += 1;
                    self.blocks(blocks, cx, w - (cx - x), color, true);
                    self.lists -= 1;
                    if self.y == before {
                        self.y += first_h;
                    }
                }
            }
            Block::Table { aligns, head, rows } => self.table(aligns, head, rows, x, w),
            Block::Rule => {
                self.y += (size * 0.4).round();
                self.pieces.push(Piece::Line { a: pos2(x, self.y), b: pos2(x + w, self.y), col: pal.border });
                self.y += (size * 0.4).round();
            }
            Block::Images(list) => {
                let mut row_x = x;
                let mut row_h: f32 = 0.0;
                for &(img, link) in list {
                    let Some(im) = self.images.get(img as usize - 1) else { continue };
                    match self.pics.get(img as usize - 1).copied().flatten() {
                        Some(sz) => {
                            let k = (w / sz.x).min(1.0);
                            let s = sz * k;
                            if row_x > x && row_x + s.x > x + w {
                                self.y += row_h + 4.0;
                                row_x = x;
                                row_h = 0.0;
                            }
                            self.pieces.push(Piece::Picture { r: Rect::from_min_size(pos2(row_x, self.y), s), img, link });
                            row_x += s.x + 4.0;
                            row_h = row_h.max(s.y);
                        }
                        None => {
                            // Not on disk (or not yet decoded): its description, as a link
                            // when it is one.
                            let alt = if im.alt.is_empty() { im.src.rsplit('/').next().unwrap_or("picture").to_string() } else { im.alt.clone() };
                            let run = Run { text: alt, st: 0, link };
                            let (job, links, _) = self.job(std::slice::from_ref(&run), (size * 0.92).round(), pal.text_dim, w, false, false);
                            let g = self.ui.fonts_mut(|f| f.layout_job(job));
                            if row_x > x && row_x + g.size().x + 12.0 > x + w {
                                self.y += row_h + 4.0;
                                row_x = x;
                                row_h = 0.0;
                            }
                            let r = Rect::from_min_size(pos2(row_x, self.y), g.size() + vec2(12.0, 6.0));
                            self.pieces.push(Piece::Fill { r, col: Color32::TRANSPARENT, round: 4, line: Some(pal.card_line()) });
                            self.pieces.push(Piece::Text { pos: r.min + vec2(6.0, 3.0), g, bold: None, links });
                            row_x = r.right() + 4.0;
                            row_h = row_h.max(r.height());
                        }
                    }
                }
                self.y += row_h;
            }
        }
    }

    fn table(&mut self, aligns: &[Align], head: &[Vec<Run>], rows: &[Vec<Vec<Run>>], x: f32, w: f32) {
        let pal = self.st.pal;
        let size = self.st.size;
        let pad = 8.0;
        let n = aligns.len();
        if n == 0 {
            return;
        }
        // Natural widths, shrunk in proportion (never below a few characters) to fit.
        let natural: Vec<f32> = (0..n)
            .map(|c| {
                std::iter::once(&head[c]).chain(rows.iter().map(|r| &r[c])).take(200).map(|runs| {
                    let (mut job, ..) = self.job(runs, size, pal.text, f32::INFINITY, false, false);
                    job.wrap.max_width = f32::INFINITY;
                    self.ui.fonts_mut(|f| f.layout_job(job)).size().x + pad * 2.0
                }).fold(size * 2.0, f32::max)
            })
            .collect();
        let sum: f32 = natural.iter().sum();
        let min_w = (size * 3.0).min(w / n as f32);
        let widths: Vec<f32> = if sum <= w { natural } else { natural.iter().map(|nw| (nw * w / sum).max(min_w)).collect() };
        let total: f32 = widths.iter().sum::<f32>().min(w.max(min_w * n as f32));
        let top = self.y;
        let row = |me: &mut Self, cells: &[Vec<Run>], header: bool| {
            let y0 = me.y;
            let mut h: f32 = 0.0;
            let mut cx = x;
            let mut placed = vec![];
            for (c, runs) in cells.iter().enumerate() {
                let cw = widths[c];
                let (job, links, any_bold) = me.job(runs, size, pal.text, cw - pad * 2.0, header, false);
                let g = me.ui.fonts_mut(|f| f.layout_job(job));
                let bold = any_bold.then(|| {
                    let (job, ..) = me.job(runs, size, pal.text, cw - pad * 2.0, header, true);
                    me.ui.fonts_mut(|f| f.layout_job(job))
                });
                let gx = match aligns[c] {
                    Align::Left => cx + pad,
                    Align::Center => cx + (cw - g.size().x) / 2.0,
                    Align::Right => cx + cw - pad - g.size().x,
                };
                h = h.max(g.size().y);
                placed.push((gx, g, bold, links));
                cx += cw;
            }
            let rh = h + pad;
            if header {
                me.pieces.push(Piece::Fill { r: Rect::from_min_size(pos2(x, y0), vec2(total, rh)), col: pal.card(), round: 0, line: None });
            }
            for (gx, g, bold, links) in placed {
                me.pieces.push(Piece::Text { pos: pos2(gx, y0 + pad / 2.0), g, bold, links });
            }
            me.y = y0 + rh;
            me.pieces.push(Piece::Line { a: pos2(x, me.y), b: pos2(x + total, me.y), col: if header { pal.border } else { pal.card_line() } });
        };
        row(self, head, true);
        for r in rows {
            row(self, r, false);
        }
        self.pieces.push(Piece::Fill { r: Rect::from_min_max(pos2(x, top), pos2(x + total, self.y)), col: Color32::TRANSPARENT, round: 4, line: Some(pal.card_line()) });
    }
}

/// The laid-out text pieces in reading order: where each was put, and its galley.
fn texts(pieces: &[Piece]) -> impl Iterator<Item = (Pos2, &Arc<Galley>)> {
    pieces.iter().filter_map(|p| match p {
        Piece::Text { pos, g, .. } => Some((*pos, g)),
        _ => None,
    })
}

/// The byte of `t` where character `c` starts.
fn byte_of(t: &str, c: usize) -> usize {
    t.char_indices().nth(c).map_or(t.len(), |(b, _)| b)
}

/// The place in the document nearest `q` (document coordinates): in a piece on its line,
/// else the end of the last piece above it, else the start of the first.
fn hit(pieces: &[Piece], q: Pos2) -> crate::textsel::At {
    let (mut best, mut above): (Option<(f32, crate::textsel::At)>, Option<crate::textsel::At>) = (None, None);
    for (k, (pos, g)) in texts(pieces).enumerate() {
        let r = Rect::from_min_size(pos, g.size());
        if q.y >= r.top() && q.y <= r.bottom() {
            let dx = if q.x < r.left() { r.left() - q.x } else { (q.x - r.right()).max(0.0) };
            if best.is_none_or(|b| dx < b.0) {
                best = Some((dx, (k, g.cursor_from_pos(q - pos).index)));
            }
        } else if r.bottom() < q.y {
            above = Some((k, g.text().chars().count()));
        }
    }
    best.map(|b| b.1).or(above).unwrap_or((0, 0))
}

/// Paint the selected characters `a..b` of a galley drawn at `at`; `on` the selection
/// goes on past them (its line break shows as a sliver).
fn paint_sel(p: &egui::Painter, at: Pos2, g: &Galley, a: usize, b: usize, on: bool, pal: &Palette) {
    let mut c = 0;
    let last = g.rows.len().saturating_sub(1);
    for (i, row) in g.rows.iter().enumerate() {
        let n = row.char_count_excluding_newline();
        let (x, y) = (a.max(c), b.min(c + n));
        let tail = (b > c + n || on && b >= c + n) && (row.ends_with_newline || i == last);
        if x < y || tail && x <= c + n {
            let rr = row.rect().translate(at.to_vec2());
            let x0 = at.x + row.pos.x + row.x_offset(x - c);
            let x1 = at.x + row.pos.x + row.x_offset(y.max(x) - c) + if tail { 4.0 } else { 0.0 };
            crate::textsel::paint(p, Rect::from_min_max(pos2(x0, rr.top()), pos2(x1, rr.bottom())), pal);
        }
        c += row.char_count_including_newline();
    }
}

/// What a click on a link asks for.
pub enum Follow {
    Url(String),
    Open(PathBuf),
    Folder(PathBuf),
}

/// Draw the document; returns a link the user clicked.
pub fn view(ui: &mut Ui, body: Rect, v: &mut View, size: f32, pal: &Palette, spacing: f32, thumbs: &mut crate::thumbs::Thumbs, id: u64) -> Option<Follow> {
    let ppp = ui.ctx().pixels_per_point();
    let margin = (size * 1.1).round().clamp(10.0, 18.0);
    let w = (body.width() - margin * 2.0 - 12.0).max(40.0).min(900.0);
    // Pictures as they load: their size in points, shrunk to the width.
    let pics: Vec<Option<Vec2>> = v
        .doc
        .images
        .iter()
        .map(|im| {
            let (p, m) = im.path.as_ref()?;
            let l = thumbs.get(p, *m, (w * ppp).min(1600.0))?;
            Some(vec2(l.dims[0] as f32, l.dims[1] as f32) / ppp)
        })
        .collect();
    let loaded = pics.iter().filter(|p| p.is_some()).count() as u32;
    // Formulas as TeX hands them back: a 10 pt em drawn at the text's size.
    v.math.pump(ui.ctx(), &v.doc.maths);
    let scale = size * 1.08 / (crate::texmath::DPI as f32 * 10.0 / 72.27);
    let maths: Vec<Option<MathPic>> = (0..v.doc.maths.len()).map(|i| v.math.got.get(i).and_then(|g| g.as_ref().ok()).map(|(t, px, d)| MathPic { tex: t.id(), size: *px * scale, depth: d * scale })).collect();
    let typeset = maths.iter().filter(|m| m.is_some()).count() as u32;
    let key = (w.round() as u32, (size * 10.0) as u32, (spacing * 100.0) as u32, pal.text.to_array(), pal.accent.to_array(), loaded + (typeset << 16));
    let atlas = ui.fonts(|f| f.font_atlas_fill_ratio());
    if v.laid.as_ref().is_none_or(|l| l.key != key || atlas < l.atlas) {
        v.laid = Some(Laid { key, pieces: vec![], height: 0.0, anchors: vec![], next: 0, atlas });
    }
    if let Some(l) = v.laid.as_mut() {
        l.atlas = atlas;
    }
    // Laid out from the top a few milliseconds a frame: a long document shows its start
    // at once and the rest follows, without a stalled frame.
    if let Some(l) = v.laid.as_mut().filter(|l| l.next <= v.doc.blocks.len()) {
        let t0 = std::time::Instant::now();
        let mut wr = Writer { ui, st: Style { size, spacing, pal }, pieces: std::mem::take(&mut l.pieces), anchors: std::mem::take(&mut l.anchors), y: l.height, pics: &pics, images: &v.doc.images, maths: &maths, sources: &v.doc.maths, lists: 0 };
        // Why the formulas show as written, once, above them all.
        if l.next == 0 {
            if let Some(n) = &v.math.note {
                let run = Run { text: n.clone(), st: EM, link: 0 };
                let h = wr.text(std::slice::from_ref(&run), 0.0, w, size - 1.0, pal.text_dim, false);
                wr.y += h + wr.gap();
            }
        }
        while l.next < v.doc.blocks.len() && t0.elapsed() < std::time::Duration::from_millis(6) {
            if l.next > 0 {
                wr.y += wr.gap();
            }
            wr.block(&v.doc.blocks[l.next], 0.0, w, pal.text, false);
            l.next += 1;
        }
        if l.next == v.doc.blocks.len() {
            if v.doc.cut {
                wr.y += wr.gap();
                let run = Run { text: "The rest of this document is shown in Source.".into(), st: EM, link: 0 };
                let h = wr.text(std::slice::from_ref(&run), 0.0, w, size, pal.text_dim, false);
                wr.y += h;
            }
            l.next += 1;
        } else {
            ui.ctx().request_repaint();
        }
        l.height = wr.y;
        l.pieces = wr.pieces;
        l.anchors = wr.anchors;
    }
    let Some(laid) = &v.laid else { return None };
    let mut follow = None;
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body));
    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("md", id));
    if let Some(y) = v.jump.take() {
        area = area.vertical_scroll_offset(y);
    }
    area.show_viewport(&mut inner, |ui, vp| {
        let (r, resp) = ui.allocate_exact_size(vec2(vp.width(), laid.height + margin * 2.0), Sense::click_and_drag());
        let origin = r.min + vec2(margin, margin);
        v.origin = origin;
        let pieces = &laid.pieces;
        let piece_text = |k: usize| texts(pieces).nth(k).map(|(_, g)| g.text()).unwrap_or("");
        let word = |(k, c): crate::textsel::At| {
            let t = piece_text(k);
            let (x, y) = crate::textsel::word(t, byte_of(t, c));
            ((k, t[..x].chars().count()), (k, t[..y].chars().count()))
        };
        // A line of a code block, else the whole paragraph.
        let line = |(k, c): crate::textsel::At| {
            let t = piece_text(k);
            let b = byte_of(t, c);
            let x = t[..b].rfind('\n').map_or(0, |i| i + 1);
            let y = t[b..].find('\n').map_or(t.len(), |i| b + i);
            ((k, t[..x].chars().count()), (k, t[..y].chars().count()))
        };
        crate::textsel::track(ui, &resp, &mut v.sel, |pp| hit(pieces, pp - origin.to_vec2()), word, line);
        let sel = v.sel.filter(|s| !s.is_empty()).map(|s| s.span());
        let mut k = 0;
        let vis = vp.translate(r.min.to_vec2()).expand(40.0);
        let p = ui.painter();
        let hover = resp.hover_pos();
        let tex = ui.fonts(|f| f.font_image_size());
        let mut over: Option<u32> = None;
        for piece in &laid.pieces {
            let tk = matches!(piece, Piece::Text { .. }).then(|| {
                k += 1;
                k - 1
            });
            let pr = piece.rect().translate(origin.to_vec2());
            if !pr.intersects(vis) {
                continue;
            }
            match piece {
                Piece::Text { pos, g, bold, links } => {
                    let at = origin + pos.to_vec2();
                    if let (Some(((k0, c0), (k1, c1))), Some(tk)) = (sel, tk) {
                        if (k0..=k1).contains(&tk) {
                            let n = g.text().chars().count();
                            paint_sel(p, at, g, if tk == k0 { c0 } else { 0 }, if tk == k1 { c1.min(n) } else { n }, tk < k1, pal);
                        }
                    }
                    p.galley(at, g.clone(), pal.text);
                    if let Some(b) = bold {
                        p.add(faux_bold(b, at + vec2((size / 24.0).clamp(0.45, 0.9), 0.0), tex));
                    }
                    if let (Some(h), false) = (hover, links.is_empty()) {
                        if pr.contains(h) {
                            let cc = g.cursor_from_pos(h - at);
                            let byte = g.text().char_indices().nth(cc.index).map_or(g.text().len(), |(b, _)| b);
                            // The cursor lands between characters: the one after it, or
                            // before it at a row's end.
                            if let Some((_, l)) = links.iter().find(|(rg, _)| rg.contains(&byte) || (byte > 0 && rg.contains(&(byte - 1)) && rg.end == byte)) {
                                over = Some(*l);
                            }
                        }
                    }
                }
                Piece::Fill { r, col, round, line } => {
                    let rr = r.translate(origin.to_vec2());
                    p.rect_filled(rr, CornerRadius::same(*round), *col);
                    if let Some(l) = line {
                        p.rect_stroke(rr, CornerRadius::same(*round), Stroke::new(1.0, *l), StrokeKind::Inside);
                    }
                }
                Piece::Line { a, b, col } => {
                    p.line_segment([origin + a.to_vec2(), origin + b.to_vec2()], Stroke::new(1.0, *col));
                }
                Piece::Dot { c, r, col, hollow } => {
                    if *hollow {
                        p.circle_stroke(origin + c.to_vec2(), *r, Stroke::new(1.0, *col));
                    } else {
                        p.circle_filled(origin + c.to_vec2(), *r, *col);
                    }
                }
                Piece::Check { r, done } => {
                    let rr = r.translate(origin.to_vec2());
                    if *done {
                        p.rect_filled(rr, CornerRadius::same(3), pal.accent);
                        let s = rr.width();
                        let pt = |x: f32, y: f32| rr.min + vec2(x * s, y * s);
                        p.add(egui::Shape::line(vec![pt(0.24, 0.52), pt(0.42, 0.7), pt(0.76, 0.32)], Stroke::new((s / 8.0).max(1.4), Color32::WHITE)));
                    } else {
                        p.rect_stroke(rr, CornerRadius::same(3), Stroke::new(1.2, pal.text_dim), StrokeKind::Inside);
                    }
                }
                Piece::Math { r, tex } => {
                    p.image(*tex, r.translate(origin.to_vec2()), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), pal.text);
                }
                Piece::Picture { r, img, link } => {
                    let rr = r.translate(origin.to_vec2());
                    if let Some((path, m)) = v.doc.images.get(*img as usize - 1).and_then(|im| im.path.as_ref()) {
                        if let Some(l) = thumbs.get(path, *m, (w * ppp).min(1600.0)) {
                            crate::inspector::checkerboard(p, rr, pal);
                            p.image(l.tex.id(), rr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        }
                    }
                    if *link > 0 && hover.is_some_and(|h| rr.contains(h)) {
                        over = Some(*link);
                    }
                }
            }
        }
        if let Some(l) = over.and_then(|l| v.doc.links.get(l as usize - 1)) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                if let Some(a) = l.url.strip_prefix('#') {
                    // A heading of this document: scroll to it.
                    let a = percent_decode(a).to_lowercase();
                    v.jump = laid.anchors.iter().find(|(s, _)| *s == a).map(|(_, y)| *y);
                } else if let Some(p) = &l.path {
                    follow = Some(if l.dir { Follow::Folder(p.clone()) } else { Follow::Open(p.clone()) });
                } else {
                    follow = Some(Follow::Url(l.url.clone()));
                }
            }
            resp.on_hover_text(l.url.clone());
        }
    });
    if v.jump.is_some() {
        ui.ctx().request_repaint();
    }
    follow
}

/// The strong parts of a paragraph drawn again a fraction of a point to the right,
/// thickening them: the system's regular face stands in for a bold one. A mesh, not
/// text, so it is not a second copy of the words.
fn faux_bold(g: &Galley, at: Pos2, tex: [usize; 2]) -> egui::Shape {
    let mut mesh = egui::Mesh::default();
    let (tw, th) = (tex[0].max(1) as f32, tex[1].max(1) as f32);
    for row in &g.rows {
        let m = &row.row.visuals.mesh;
        let base = mesh.vertices.len() as u32;
        let off = at.to_vec2() + row.pos.to_vec2();
        mesh.vertices.extend(m.vertices.iter().map(|v| egui::epaint::Vertex { pos: v.pos + off, uv: pos2(v.uv.x / tw, v.uv.y / th), color: v.color }));
        mesh.indices.extend(m.indices.iter().map(|i| i + base));
    }
    egui::Shape::mesh(mesh)
}

/// A link's target outside Maia: web and mail addresses only.
pub fn is_web(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://") || l.starts_with("mailto:")
}
