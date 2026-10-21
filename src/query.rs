//! The filter's language, also what smart folders keep. Plain words are a fuzzy search
//! of the name and the path below the folder. The rest are conditions: `size>100M`,
//! `modified<7d`, `created:today`, `accessed>1y`, `tag:red`, `type:pictures,videos`,
//! `ext:jpg,png`, `owner:root`, `name:IMG_*`, `*.rs`, `/^img_\d+/` or `re:...`, and a
//! "quoted phrase". Side by side they must all hold; `|` or `OR` between them is either,
//! `-`, `!` or `NOT` before one is not, and in a query with an OR parentheses group.
//! Ages count back from now; dates are local days, months or years.
//!
//! A word that only looks like a condition (`a:b`, `size>huge`) stays a word, as does an
//! unclosed `/` or a lone `-`. A pattern that is not a regular expression matches nothing
//! and shows in the danger colour, so an empty list says why.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsx::Entry;
use crate::fuzzy;
use crate::theme::FileCat;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Size,
    Modified,
    Created,
    Accessed,
}

/// A field kept inside [lo, hi): bytes, or seconds since the epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cond {
    field: Field,
    lo: i64,
    hi: i64,
}

enum Node {
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
    /// The plain words of one group, fuzzy over the name and the path below the folder.
    Words(fuzzy::Query),
    /// Lowercase text found as typed in the name or the path below the folder: a phrase
    /// (part of what is searched for, so contents mode takes it) or a word after a NOT.
    Has(String, bool),
    /// Part of the name alone (`name:`), or a pattern with * and ? for the whole name.
    Name(String),
    /// The pattern, and as bytes when it is plain ASCII.
    Glob(Vec<char>, Option<Vec<u8>>),
    /// With text every name it matches holds, so most names skip the expression.
    Re(regex_lite::Regex, Vec<String>),
    /// A pattern that did not compile.
    Never,
    Cond(Cond),
    Tag(String),
    /// Endings with their dot (".jpg", ".tar.gz").
    Ext(Vec<String>),
    /// Kinds of file, folders, files, or words of the type's description ("png image").
    /// `id` names this query in each thread's memory of which extensions matched.
    Kind { cats: Vec<FileCat>, dirs: bool, files: bool, labels: Vec<String>, id: u64 },
    Owner(Vec<u32>),
}

#[derive(Default)]
pub struct Filter {
    /// The words searched for (no conditions, nothing after a NOT): what contents mode
    /// looks for and the letters lit in the names.
    pub name: String,
    root: Option<Node>,
    /// Plain words to rank by.
    words: bool,
    conds: bool,
    /// An `owner:` waiting for the list of users, read off this thread.
    pub pending: bool,
    /// Byte ranges of the conditions and operators in the filter text (drawn in the accent).
    pub spans: Vec<(usize, usize)>,
    /// Byte ranges of patterns that are not regular expressions.
    pub bad: Vec<(usize, usize)>,
}

/// How the plain words and phrases are matched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Text {
    /// The name, then the path below the folder (a flattened listing), ranked.
    Full,
    /// The name alone (the tree view's search).
    Name,
    /// Not at all: contents mode looks for them inside the files.
    Off,
}

/// What a test has to go on.
pub struct Cx<'a> {
    pub stats: &'a HashMap<PathBuf, crate::stats::DirStats>,
    pub text: Text,
    /// False while only the name and path are known: conditions on the rest are open.
    pub meta: bool,
    /// Tags read from the file itself (worker threads), not from the cache the rows fill.
    pub read_tags: bool,
}

impl<'a> Cx<'a> {
    pub fn view(stats: &'a HashMap<PathBuf, crate::stats::DirStats>, text: Text) -> Cx<'a> {
        Cx { stats, text, meta: true, read_tags: false }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum R {
    No,
    Yes(i32),
    Open,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Anything besides plain words: conditions, phrases, patterns or operators.
    pub fn has_conds(&self) -> bool {
        self.conds
    }

    /// Whether matches are ranked by how well the words fit.
    pub fn ranked(&self, cx: &Cx) -> bool {
        self.words && cx.text == Text::Full
    }

    /// `e`'s score when it matches. A folder's size is known only once worked out.
    pub fn score(&self, e: &Entry, cx: &Cx) -> Option<i32> {
        let Some(n) = &self.root else { return Some(0) };
        match eval(n, e, cx, self.ranked(cx)) {
            R::No => None,
            R::Yes(s) => Some(s),
            R::Open => Some(0),
        }
    }

    /// False only when what is known of `e` already rules it out.
    pub fn may(&self, e: &Entry, cx: &Cx) -> bool {
        self.root.as_ref().is_none_or(|n| eval(n, e, cx, false) != R::No)
    }
}

fn eval(n: &Node, e: &Entry, cx: &Cx, ranked: bool) -> R {
    let yes = |b: bool| if b { R::Yes(0) } else { R::No };
    match n {
        Node::All(v) => {
            let (mut sum, mut open) = (0, false);
            for c in v {
                match eval(c, e, cx, ranked) {
                    R::No => return R::No,
                    R::Open => open = true,
                    R::Yes(s) => sum += s,
                }
            }
            if open { R::Open } else { R::Yes(sum) }
        }
        Node::Any(v) => {
            let (mut best, mut open) = (None, false);
            for c in v {
                match eval(c, e, cx, ranked) {
                    R::Yes(s) if !ranked => return R::Yes(s),
                    R::Yes(s) => best = best.max(Some(s)),
                    R::Open => open = true,
                    R::No => {}
                }
            }
            best.map_or(if open { R::Open } else { R::No }, R::Yes)
        }
        Node::Not(c) => match eval(c, e, cx, false) {
            R::Yes(_) => R::No,
            R::No => R::Yes(0),
            R::Open => R::Open,
        },
        // Name matches rank above matches that only hit the relative path.
        Node::Words(q) => match cx.text {
            Text::Off => R::Yes(0),
            Text::Name => fuzzy::score(q, e.lname()).map_or(R::No, R::Yes),
            Text::Full => {
                let name = fuzzy::score(q, e.lname()).map(|s| s + 40);
                let path = e.rel().as_ref().and_then(|r| fuzzy::score_ci(q, r));
                name.max(path).map_or(R::No, R::Yes)
            }
        },
        Node::Has(s, text) => match cx.text {
            Text::Off if *text => R::Yes(0),
            Text::Name => yes(e.lname().contains(s.as_str())),
            _ => yes(e.lname().contains(s.as_str()) || e.rel().is_some_and(|r| contains_ci(&r, s))),
        },
        Node::Name(s) => yes(e.lname().contains(s.as_str())),
        Node::Glob(_, Some(b)) if e.lname().is_ascii() => yes(crate::smart::glob(b, e.lname().as_bytes())),
        Node::Glob(p, _) => yes(crate::smart::glob(p, &e.lname().chars().collect::<Vec<_>>())),
        Node::Re(r, need) => yes((need.is_empty() || need.iter().any(|n| e.lname().contains(n.as_str()))) && r.is_match(&e.name)),
        Node::Never => R::No,
        Node::Ext(v) => yes(!e.is_dir && v.iter().any(|x| e.lname().len() > x.len() && e.lname().ends_with(x.as_str()))),
        Node::Kind { cats, dirs, files, labels, id } => {
            if e.is_dir {
                return yes(*dirs);
            }
            if *files || cats.contains(&crate::theme::categorize(e.ext(), false, false)) {
                return R::Yes(0);
            }
            if labels.is_empty() {
                return R::No;
            }
            if !cx.meta {
                return R::Open;
            }
            let db = crate::mime::mime_db();
            let exec = e.is_executable();
            let test = || {
                let l = db.label(&e.name, e.ext(), false, exec).to_lowercase();
                labels.iter().any(|w| l.contains(w.as_str()))
            };
            if db.named(&e.name) {
                return yes(test());
            }
            // Descriptions are per extension (and whether the file runs), so each is
            // looked at once per thread, not once per file under the database's lock.
            SEEN_TYPES.with(|m| {
                let mut m = m.borrow_mut();
                if m.0 != *id {
                    *m = (*id, Default::default());
                }
                let seen = &mut m.1[exec as usize];
                if let Some(&b) = seen.get(e.ext()) {
                    return yes(b);
                }
                let b = test();
                seen.insert(e.ext().into(), b);
                yes(b)
            })
        }
        _ if !cx.meta => R::Open,
        Node::Cond(c) => {
            let v = match c.field {
                Field::Size if e.is_dir => cx.stats.get(&e.path).map(|s| s.size as i64),
                Field::Size => Some(e.size as i64),
                Field::Modified => e.modified.map(secs),
                Field::Created => e.created.map(secs),
                Field::Accessed => e.accessed.map(secs),
            };
            yes(v.is_some_and(|v| c.lo <= v && v < c.hi))
        }
        // Tags not read yet count as none for now; reading them was asked for, and the
        // view is filtered again as they land.
        Node::Tag(t) if cx.read_tags => yes(crate::tags::read(&e.path).iter().any(|x| x.eq_ignore_ascii_case(t))),
        Node::Tag(t) => yes(crate::tags::get(&e.path, |v| v.iter().any(|x| x.eq_ignore_ascii_case(t))).unwrap_or(false)),
        Node::Owner(v) => yes(v.contains(&e.uid)),
    }
}

static KIND_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// For one `type:` with a description: whether each extension's matched, for files
    /// that can't run and files that can.
    static SEEN_TYPES: std::cell::RefCell<(u64, [HashMap<Box<str>, bool>; 2])> = std::cell::RefCell::new((0, Default::default()));
}

/// Whether `hay` holds `needle` (already lowercase), in any case, without allocating
/// for plain ASCII.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        n.is_empty() || h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Any character that can start or make a condition or an operator. Text without one
/// is all plain words, and nothing has to be parsed.
pub fn marked(text: &str) -> bool {
    text.contains(['>', '<', '=', ':', '|', '(', ')', '"', '/', '*', '?', '-', '!']) || text.contains("OR") || text.contains("NOT")
}

thread_local! {
    /// The last few texts parsed for drawing: the filter box and the rows ask every frame.
    static SEEN: std::cell::RefCell<Vec<(String, std::rc::Rc<Filter>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `text` parsed, from a small cache: for what is drawn each frame, never for filtering
/// (ages there must count from now).
pub fn cached(text: &str) -> std::rc::Rc<Filter> {
    SEEN.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(i) = c.iter().position(|x| x.0 == text) {
            return c[i].1.clone();
        }
        let f = std::rc::Rc::new(parse(text, SystemTime::now()));
        if c.len() >= 4 {
            c.remove(0);
        }
        c.push((text.to_string(), f.clone()));
        f
    })
}

/// The name-search part of the filter, without allocating when there are no conditions.
pub fn name_part(text: &str) -> std::borrow::Cow<'_, str> {
    if marked(text) {
        let f = cached(text);
        if f.has_conds() {
            return std::borrow::Cow::Owned(f.name.clone());
        }
    }
    std::borrow::Cow::Borrowed(text)
}

/// The filter text as drawn: conditions and operators in the accent, broken patterns in
/// the danger colour.
pub fn job(s: &str, font: egui::FontId, text: egui::Color32, accent: egui::Color32, danger: egui::Color32) -> egui::text::LayoutJob {
    let fmt = |c: egui::Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    let mut job = egui::text::LayoutJob::default();
    let f = cached(s);
    let mut marks: Vec<(usize, usize, egui::Color32)> = f.spans.iter().map(|&(a, b)| (a, b, accent)).chain(f.bad.iter().map(|&(a, b)| (a, b, danger))).collect();
    marks.sort_by_key(|m| m.0);
    let mut at = 0;
    for (a, b, c) in marks {
        if a < at || b > s.len() {
            continue;
        }
        job.append(&s[at..a], 0.0, fmt(text));
        job.append(&s[a..b], 0.0, fmt(c));
        at = b;
    }
    job.append(&s[at..], 0.0, fmt(text));
    job
}

// ------------------------------------------------------------------ owners

static USERS: OnceLock<Vec<(u32, String)>> = OnceLock::new();
static READING: AtomicBool = AtomicBool::new(false);

/// The system's users, read once. Without `wait` the first call starts reading them on
/// another thread and returns None until they are in.
pub fn users(wait: bool) -> Option<&'static [(u32, String)]> {
    if let Some(u) = USERS.get() {
        return Some(u);
    }
    if wait {
        return Some(USERS.get_or_init(|| crate::perms::read_ids("/etc/passwd")));
    }
    if !READING.swap(true, Ordering::Relaxed) {
        let _ = std::thread::Builder::new().name("users".into()).spawn(|| {
            USERS.get_or_init(|| crate::perms::read_ids("/etc/passwd"));
        });
    }
    None
}

// ------------------------------------------------------------------ parsing

#[derive(Clone, Copy, Debug, PartialEq)]
enum T {
    Word,
    Or,
    Not,
    Open,
    Close,
}

/// Words and operators with their byte ranges. Parentheses are operators only in a
/// query with an OR, so "photo (1)" stays the name it always matched.
fn lex(s: &str) -> Vec<(T, usize, usize)> {
    let ors = s.contains('|') || s.split_whitespace().any(|w| w == "OR");
    let ch = |i: usize| s[i..].chars().next();
    let mut out = vec![];
    let mut i = 0;
    while let Some(c) = ch(i) {
        let next = ch(i + c.len_utf8());
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let op = match c {
            '|' => Some(T::Or),
            '(' if ors => Some(T::Open),
            ')' if ors => Some(T::Close),
            // Only in front of something: "my-file" and a lone "-" are words.
            '-' | '!' if next.is_some_and(|n| !n.is_whitespace() && n != '|' && n != ')') => Some(T::Not),
            _ => None,
        };
        if let Some(t) = op {
            out.push((t, i, i + 1));
            i += 1;
            continue;
        }
        // A pattern between slashes, ending where a word would.
        if c == '/' {
            let mut j = i + 1;
            let mut end = None;
            while let Some(d) = ch(j) {
                if d.is_whitespace() {
                    break;
                }
                if d == '\\' {
                    j += 1 + ch(j + 1).map_or(0, char::len_utf8);
                    continue;
                }
                if d == '/' && j > i + 1 && ch(j + 1).is_none_or(|a| a.is_whitespace() || a == '|' || ors && a == ')') {
                    end = Some(j + 1);
                    break;
                }
                j += d.len_utf8();
            }
            if let Some(end) = end {
                out.push((T::Word, i, end));
                i = end;
                continue;
            }
        }
        // A word, to the next space or `|`, with "quoted" parts kept whole; `re:` takes
        // everything to the next space.
        let re = s[i..].get(..3).is_some_and(|k| k.eq_ignore_ascii_case("re:"));
        let (start, mut j, mut quoted) = (i, i, false);
        while let Some(d) = ch(j) {
            if d == '"' {
                quoted = !quoted;
            } else if !quoted && (d.is_whitespace() || d == '|' && !re) {
                break;
            }
            j += d.len_utf8();
        }
        // Closing parentheses at its end that it did not open itself.
        let mut end = j;
        if ors {
            let w = &s[start..j];
            let mut extra = w.matches(')').count().saturating_sub(w.matches('(').count());
            while extra > 0 && end > start + 1 && s[start..end].ends_with(')') {
                end -= 1;
                extra -= 1;
            }
        }
        let t = match &s[start..end] {
            "OR" => T::Or,
            "NOT" => T::Not,
            _ => T::Word,
        };
        out.push((t, start, end));
        out.extend((end..j).map(|k| (T::Close, k, k + 1)));
        i = j;
    }
    out
}

enum Part {
    Word(usize, usize),
    Node(Node),
}

struct Parser<'a> {
    s: &'a str,
    toks: Vec<(T, usize, usize)>,
    k: usize,
    now: i64,
    f: Filter,
    /// The words and phrases searched for, in order.
    said: Vec<String>,
}

pub fn parse(text: &str, now: SystemTime) -> Filter {
    let mut p = Parser { s: text, toks: lex(text), k: 0, now: secs(now), f: Filter::default(), said: vec![] };
    // A `)` with nothing open is passed over.
    let mut parts = vec![];
    loop {
        if let Some(n) = p.any(false) {
            parts.push(n);
        }
        if p.k >= p.toks.len() {
            break;
        }
        p.k += 1;
    }
    p.f.root = match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Node::All(parts)),
    };
    p.f.name = p.said.join(" ");
    p.f.spans.sort_unstable();
    p.f
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<T> {
        self.toks.get(self.k).map(|t| t.0)
    }

    fn mark(&mut self) {
        let (_, a, b) = self.toks[self.k];
        self.f.spans.push((a, b));
        self.f.conds = true;
        self.k += 1;
    }

    /// Alternatives split by OR; an empty one (while typing "a |") is left out.
    fn any(&mut self, neg: bool) -> Option<Node> {
        let mut alts = vec![];
        loop {
            alts.extend(self.all(neg));
            if self.peek() != Some(T::Or) {
                break;
            }
            self.mark();
        }
        match alts.len() {
            0 => None,
            1 => alts.pop(),
            _ => Some(Node::Any(alts)),
        }
    }

    /// Terms side by side. Their plain words are one fuzzy search, as they always were;
    /// inside a NOT each word is matched as typed.
    fn all(&mut self, neg: bool) -> Option<Node> {
        let s = self.s;
        let (mut nodes, mut words) = (vec![], vec![]);
        while let Some(t) = self.peek() {
            if t == T::Or || t == T::Close {
                break;
            }
            match self.unary(neg) {
                Some(Part::Word(a, b)) if neg => nodes.push(Node::Has(s[a..b].to_lowercase(), false)),
                Some(Part::Word(a, b)) => words.push(&s[a..b]),
                Some(Part::Node(n)) => nodes.push(n),
                None => {}
            }
        }
        if !words.is_empty() {
            self.f.words = true;
            nodes.push(Node::Words(fuzzy::Query::new(&words.join(" "))));
        }
        // The cheap tests first: most items fail one of them.
        nodes.sort_by_key(cost);
        match nodes.len() {
            0 => None,
            1 => nodes.pop(),
            _ => Some(Node::All(nodes)),
        }
    }

    fn unary(&mut self, neg: bool) -> Option<Part> {
        let (t, a, b) = *self.toks.get(self.k)?;
        match t {
            T::Not => {
                self.mark();
                if matches!(self.peek(), None | Some(T::Or | T::Close)) {
                    return None;
                }
                let inner = match self.unary(!neg)? {
                    Part::Word(a, b) => Node::Has(self.s[a..b].to_lowercase(), false),
                    Part::Node(n) => n,
                };
                Some(Part::Node(Node::Not(Box::new(inner))))
            }
            T::Open => {
                self.mark();
                let e = self.any(neg);
                if self.peek() == Some(T::Close) {
                    self.mark();
                }
                e.map(Part::Node)
            }
            T::Word => {
                self.k += 1;
                self.word(a, b, neg)
            }
            T::Or | T::Close => None,
        }
    }

    fn word(&mut self, a: usize, b: usize, neg: bool) -> Option<Part> {
        let w = &self.s[a..b];
        let node = match self.atom(w) {
            Some(Ok(n)) => n,
            Some(Err(())) => {
                self.f.bad.push((a, b));
                self.f.conds = true;
                return Some(Part::Node(Node::Never));
            }
            None => {
                if neg {
                    self.f.spans.push((a, b));
                } else {
                    self.said.push(w.to_string());
                }
                return Some(Part::Word(a, b));
            }
        };
        self.f.spans.push((a, b));
        self.f.conds = true;
        Some(Part::Node(match node {
            // A phrase is searched for, like the words, unless it is after a NOT.
            Node::Has(p, _) if neg => Node::Has(p, false),
            Node::Has(p, _) => {
                self.said.push(w.replace('"', ""));
                Node::Has(p, true)
            }
            n => n,
        }))
    }

    /// What a word means: a condition, a broken pattern, or (None) a plain word.
    fn atom(&mut self, w: &str) -> Option<Result<Node, ()>> {
        let unq = |v: &str| v.replace('"', "");
        let re = |p: &str| {
            // Flags could turn case back on, or let a dot reach further: no shortcut then.
            let need = (!p.contains("(?")).then(|| crate::codesearch::required_literals(p)).flatten().unwrap_or_default();
            let need = if need.iter().all(|n| n.is_ascii()) { need.iter().map(|n| n.to_ascii_lowercase()).collect() } else { vec![] };
            regex_lite::RegexBuilder::new(p).case_insensitive(true).build().map(|r| Node::Re(r, need)).map_err(|_| ())
        };
        if w.len() >= 3 && w.starts_with('/') && w.ends_with('/') {
            return Some(re(&w[1..w.len() - 1]));
        }
        if let Some((k, v)) = w.split_once(':').filter(|(_, v)| !unq(v).trim().is_empty()) {
            let list = || unq(v).split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
            match k.to_ascii_lowercase().as_str() {
                "tag" => return Some(Ok(Node::Tag(unq(v)))),
                "re" => return Some(re(&unq(v))),
                "name" => {
                    let v = unq(v).to_lowercase();
                    return Some(Ok(if v.contains(['*', '?']) { glob(&v) } else { Node::Name(v) }));
                }
                "ext" => return Some(Ok(Node::Ext(list().into_iter().map(|x| format!(".{}", x.trim_start_matches('.'))).collect()))),
                "type" | "kind" => {
                    let (mut cats, mut dirs, mut files, mut labels) = (vec![], false, false, vec![]);
                    for x in list() {
                        match x.as_str() {
                            "folder" | "folders" | "dir" | "dirs" => dirs = true,
                            "file" | "files" => files = true,
                            _ => match kind_cats(&x) {
                                Some(c) => cats.extend_from_slice(c),
                                None => labels.push(x),
                            },
                        }
                    }
                    return Some(Ok(Node::Kind { cats, dirs, files, labels, id: KIND_IDS.fetch_add(1, Ordering::Relaxed) }));
                }
                "owner" | "user" => {
                    let mut ids = vec![];
                    for x in list() {
                        match x.parse::<u32>() {
                            Ok(id) => ids.push(id),
                            Err(_) => match users(false) {
                                Some(u) => ids.extend(u.iter().filter(|u| u.1.eq_ignore_ascii_case(&x)).map(|u| u.0)),
                                None => self.f.pending = true,
                            },
                        }
                    }
                    return Some(Ok(Node::Owner(ids)));
                }
                _ => {}
            }
        }
        if let Some(c) = cond(w, self.now) {
            return Some(Ok(Node::Cond(c)));
        }
        if w.contains('"') {
            let p = unq(w).to_lowercase();
            return (!p.trim().is_empty()).then_some(Ok(Node::Has(p, true)));
        }
        if w.contains(['*', '?']) {
            return Some(Ok(glob(&w.to_lowercase())));
        }
        None
    }
}

fn glob(p: &str) -> Node {
    Node::Glob(p.chars().collect(), p.is_ascii().then(|| p.as_bytes().to_vec()))
}

fn cost(n: &Node) -> u8 {
    match n {
        Node::Cond(_) | Node::Ext(_) | Node::Owner(_) | Node::Name(_) | Node::Never => 0,
        Node::Kind { labels, .. } if labels.is_empty() => 0,
        Node::Words(_) | Node::Has(..) | Node::Glob(..) => 1,
        Node::Re(..) | Node::All(_) | Node::Any(_) | Node::Not(_) => 2,
        Node::Tag(_) | Node::Kind { .. } => 3,
    }
}

/// The kinds `type:` knows, the smart folders' among them, as the categories they hold.
pub fn kind_cats(kind: &str) -> Option<&'static [FileCat]> {
    Some(match kind {
        "pictures" | "picture" | "images" | "image" | "photos" => &[FileCat::Image],
        "documents" | "document" | "docs" => &[FileCat::Doc, FileCat::Text, FileCat::Sheet, FileCat::Slides, FileCat::Pdf, FileCat::Book],
        "pdfs" | "pdf" => &[FileCat::Pdf],
        "videos" | "video" | "movies" => &[FileCat::Video],
        "music" | "audio" | "sound" => &[FileCat::Audio],
        "archives" | "archive" => &[FileCat::Archive],
        "code" => &[FileCat::Code],
        "text" => &[FileCat::Text],
        "spreadsheets" | "sheets" => &[FileCat::Sheet],
        "presentations" | "slides" => &[FileCat::Slides],
        "books" | "ebooks" => &[FileCat::Book],
        "fonts" | "font" => &[FileCat::Font],
        "disks" | "disk" => &[FileCat::Disk],
        _ => return None,
    })
}

fn cond(w: &str, now: i64) -> Option<Cond> {
    let split = w.find(['>', '<', '=', ':'])?;
    let field = match w[..split].to_ascii_lowercase().as_str() {
        "size" => Field::Size,
        "modified" | "mod" | "date" => Field::Modified,
        "created" => Field::Created,
        "accessed" => Field::Accessed,
        _ => return None,
    };
    let rest = &w[split..];
    let op = ["<=", ">=", "<", ">", "=", ":"].into_iter().find(|o| rest.starts_with(o))?;
    let v = &rest[op.len()..];
    let (lo, hi) = match field {
        Field::Size => {
            let n = size(v)?;
            match op {
                ">" => (n + 1, i64::MAX),
                ">=" => (n, i64::MAX),
                "<" => (0, n),
                "<=" => (0, n + 1),
                _ => (n, n + 1),
            }
        }
        _ => match age(v) {
            // An age: newer than it with "<" (and ":" or "="), older with ">".
            Some(a) => match op {
                ">" | ">=" => (i64::MIN, now - a),
                _ => (now - a, i64::MAX),
            },
            None => {
                let (a, b) = when(v, now)?;
                match op {
                    ">" => (b, i64::MAX),
                    ">=" => (a, i64::MAX),
                    "<" => (i64::MIN, a),
                    "<=" => (i64::MIN, b),
                    _ => (a, b),
                }
            }
        },
    };
    Some(Cond { field, lo, hi })
}

/// "100M", "1.5g", "20kb", "0": binary units, like the sizes shown in the list.
pub(crate) fn size(v: &str) -> Option<i64> {
    let v = v.to_ascii_lowercase();
    let digits = v.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(v.len());
    let n: f64 = v[..digits].parse().ok()?;
    let unit = match v[digits..].trim_end_matches(['b', 'i']) {
        "" => 1u64,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    Some((n * unit as f64).round() as i64)
}

/// The seconds a filter's date form spans, for the history's `since:` and `until:`: an
/// age is the moment that long ago (both ends), a day, month or year its local start and
/// end. The flag says it was an age.
pub(crate) fn bounds(v: &str, now: i64) -> Option<(i64, i64, bool)> {
    match age(v) {
        Some(a) => Some((now - a, now - a, true)),
        None => when(v, now).map(|(a, b)| (a, b, false)),
    }
}

/// "30min", "12h", "7d", "2w", "3mo", "1y" in seconds.
fn age(v: &str) -> Option<i64> {
    let digits = v.find(|c: char| !c.is_ascii_digit()).filter(|&d| d > 0)?;
    let n: i64 = v[..digits].parse().ok()?;
    let unit = match v[digits..].to_ascii_lowercase().as_str() {
        "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "mo" => 30 * 86_400,
        "y" => 365 * 86_400,
        _ => return None,
    };
    Some(n * unit)
}

/// A local day, month or year ("2026-10-14", "2026-10", "2026", "today",
/// "yesterday") as the seconds it spans.
fn when(v: &str, now: i64) -> Option<(i64, i64)> {
    let start_of = |y: i32, m: i32, d: i32| -> Option<i64> {
        crate::timefmt::parse_local(&format!("{y:04}-{m:02}-{d:02}T00:00:00")).map(secs)
    };
    let day = |off: i64| -> Option<(i64, i64)> {
        let tm = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((now - off * 86_400).max(0) as u64));
        let a = start_of(tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)?;
        // The next local midnight (a day is 23 or 25 hours across a clock change).
        let next = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((a + 30 * 3600) as u64));
        Some((a, start_of(next.tm_year + 1900, next.tm_mon + 1, next.tm_mday)?))
    };
    match v.to_ascii_lowercase().as_str() {
        "today" => return day(0),
        "yesterday" => return day(1),
        _ => {}
    }
    let n: Vec<i32> = v.split('-').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    match n[..] {
        [y] if v.len() == 4 => Some((start_of(y, 1, 1)?, start_of(y + 1, 1, 1)?)),
        [y, m] if (1..=12).contains(&m) => Some((start_of(y, m, 1)?, if m == 12 { start_of(y + 1, 1, 1)? } else { start_of(y, m + 1, 1)? })),
        [y, m, d] if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            let a = start_of(y, m, d)?;
            let next = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((a + 30 * 3600).max(0) as u64));
            Some((a, start_of(next.tm_year + 1900, next.tm_mon + 1, next.tm_mday)?))
        }
        _ => None,
    }
}
