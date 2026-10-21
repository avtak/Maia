//! Searching the history: what is typed in its find bar, as a narrower `git log`.
//!
//! `author:`, `path:`, `since:` and `until:` become git's own flags, so git never hands
//! over what they rule out. The other words are looked for in each commit's message,
//! author, email and hash as the log streams in: git ANDs `--grep` with `--author`, so it
//! can't find a word that is in either. Every word must be found somewhere; several
//! `author:` or `path:` terms find any of them. Upper case anywhere matches case, as in
//! the preview's find bar.

use crate::gitlog::Commit;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub words: Vec<String>,
    pub authors: Vec<String>,
    pub paths: Vec<String>,
    /// Seconds since the epoch: from, and up to (both included).
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub case: bool,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.authors.is_empty() && self.paths.is_empty() && self.since.is_none() && self.until.is_none()
    }

    /// git's flags for it, before the `--`.
    pub fn flags(&self) -> Vec<String> {
        let mut v = vec![];
        if !self.authors.is_empty() {
            v.push("--fixed-strings".into());
            if !self.case {
                v.push("--regexp-ignore-case".into());
            }
            v.extend(self.authors.iter().map(|a| format!("--author={a}")));
        }
        v.extend(self.since.map(|t| format!("--since=@{t}")));
        v.extend(self.until.map(|t| format!("--until=@{t}")));
        v
    }
}

/// Words, with "double quotes" keeping spaces in one (`author:"Ada L"`, `"two words"`).
fn tokens(text: &str) -> Vec<String> {
    let (mut v, mut cur, mut quoted, mut any) = (vec![], String::new(), false, false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    v.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        v.push(cur);
    }
    v.retain(|w| !w.is_empty());
    v
}

/// A path as git takes it from the top of the work tree.
fn clean_path(p: &str) -> String {
    let p = p.trim_start_matches("./").trim_start_matches('/').trim_end_matches('/');
    if p == "." { String::new() } else { p.to_string() }
}

pub fn parse(text: &str, now: i64) -> Query {
    let mut q = Query::default();
    for w in tokens(text) {
        let (key, val) = w.split_once(':').unwrap_or(("", ""));
        match key.to_ascii_lowercase().as_str() {
            // Half typed ("author:"): nothing yet.
            "author" | "path" | "since" | "until" if val.is_empty() => {}
            "author" => q.authors.push(val.to_string()),
            "path" => {
                let p = clean_path(val);
                if !p.is_empty() {
                    q.paths.push(p);
                }
            }
            k @ ("since" | "until") => match crate::query::bounds(val, now) {
                Some((lo, _, _)) if k == "since" => q.since = Some(lo),
                // An age's moment is the end itself; a day, month or year ends just before
                // the next one starts.
                Some((_, hi, age)) => q.until = Some(if age { hi } else { hi - 1 }),
                None => q.words.push(w),
            },
            _ => q.words.push(w),
        }
    }
    // A path or a date in capitals says nothing about the case of the words.
    q.case = q.words.iter().chain(&q.authors).any(|w| w.chars().any(char::is_uppercase));
    q
}

/// A word that could be the start of a commit's hash.
fn hashy(w: &str) -> bool {
    (4..=64).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_hexdigit())
}

fn has(hay: &str, w: &str, case: bool) -> bool {
    let mut at = vec![];
    crate::findbar::find_in(hay, w, case, &mut at);
    !at.is_empty()
}

/// Whether every word is in the commit somewhere: its subject, the rest of its message,
/// its author, their email, or the start of its hash.
pub fn matches(q: &Query, c: &Commit, body: &str) -> bool {
    q.words.iter().all(|w| has(&c.subject, w, q.case) || has(body, w, q.case) || has(&c.author, w, q.case) || has(&c.email, w, q.case) || hashy(w) && c.hash.starts_with(&w.to_ascii_lowercase()))
}

/// Where the words (and, in an author's name, the `author:` terms) are in `text`, as
/// byte ranges in order, overlaps joined: what the row marks.
pub fn spans(q: &Query, text: &str, author: bool) -> Vec<(usize, usize)> {
    let mut at = vec![];
    let terms = q.words.iter().chain(q.authors.iter().filter(|_| author));
    for w in terms {
        crate::findbar::find_in(text, w, q.case, &mut at);
    }
    at.sort_unstable();
    let mut v: Vec<(usize, usize)> = vec![];
    for (a, b) in at.into_iter().map(|(a, b)| (a as usize, b as usize)) {
        match v.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => v.push((a, b)),
        }
    }
    v
}

/// How much of the short hash a word marks.
pub fn hash_hit(q: &Query, hash: &str) -> usize {
    q.words.iter().filter(|w| hashy(w) && hash.starts_with(&w.to_ascii_lowercase())).map(|w| w.len()).max().unwrap_or(0)
}

/// What a search started from a subfolder of the work tree begins with.
pub fn start_text(rel: &str) -> String {
    match clean_path(rel) {
        p if p.is_empty() => String::new(),
        p if p.contains(char::is_whitespace) => format!("path:\"{p}\" "),
        p => format!("path:{p} "),
    }
}
