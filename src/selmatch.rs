//! Select Matching…: the entries the view shows whose names match a shell pattern
//! (`*.jpg`, `IMG_*`, `*.{png,jpg}`), added to the selection. Case is ignored: on a
//! desktop disk `IMG_0001.JPG` and `img_0002.jpg` are the same kind of file.

use crate::app::App;
use crate::fsx::DEAD;
use crate::tab::Tab;

pub struct Pattern {
    /// Each `{a,b}` alternative spelled out, lowercased, as `picker::glob` takes it.
    alts: Vec<Vec<char>>,
    /// A pattern with a slash is matched against the path below the tab's folder, so a
    /// flattened listing can be narrowed to `src/*.rs`.
    path: bool,
}

impl Pattern {
    pub fn new(s: &str) -> Option<Pattern> {
        let s = s.trim().to_lowercase();
        if s.is_empty() {
            return None;
        }
        let mut alts = vec![];
        spread(&s, &mut alts);
        Some(Pattern { path: s.contains('/'), alts: alts.into_iter().map(|a| a.chars().collect()).collect() })
    }

    /// `name` is already lowercase, in `buf` as chars.
    fn hit(&self, buf: &[char]) -> bool {
        self.alts.iter().any(|a| crate::picker::glob(a, buf))
    }
}

/// `{a,b}` groups spread into one pattern each, the first group first. A brace with no
/// comma inside is an ordinary character; past 64 alternatives the rest stay as typed.
fn spread(s: &str, out: &mut Vec<String>) {
    let group = s.find('{').and_then(|o| s[o..].find('}').map(|c| (o, o + c))).filter(|&(o, c)| s[o..c].contains(','));
    match group {
        Some((o, c)) if out.len() < 64 => {
            for part in s[o + 1..c].split(',') {
                spread(&format!("{}{part}{}", &s[..o], &s[c + 1..]), out);
            }
        }
        _ => out.push(s.to_string()),
    }
}

/// The view's entries matching `pat` added to the selection; how many match. The cursor
/// goes to the first of them unless it is on something selected already.
pub fn select(t: &mut Tab, pat: &Pattern) -> usize {
    let mut buf: Vec<char> = Vec::new();
    let mut first = None;
    let mut n = 0;
    for &i in &t.view {
        let Some(e) = t.entries.get(i as usize) else { continue };
        // Superseded rows and the tree's dimmed context rows are not entries shown.
        if e.depth == DEAD || t.dimmed.contains(&i) {
            continue;
        }
        buf.clear();
        if pat.path {
            let rel = e.path.strip_prefix(&t.path).unwrap_or(&e.path);
            buf.extend(rel.to_string_lossy().chars().flat_map(char::to_lowercase));
        } else {
            buf.extend(e.name.chars().flat_map(char::to_lowercase));
        }
        if pat.hit(&buf) {
            n += 1;
            first.get_or_insert(i);
            t.selected.insert(i);
        }
    }
    if let Some(f) = first {
        if !t.cursor.is_some_and(|c| t.selected.contains(&c)) {
            t.cursor = Some(f);
            t.anchor = Some(f);
            t.scroll_to_cursor = true;
        }
    }
    n
}

/// What the prompt was given, applied to the active tab and said.
pub fn run(app: &mut App, text: &str) {
    let Some(pat) = Pattern::new(text) else { return };
    let n = select(app.tab(), &pat);
    let text = text.trim();
    app.toast(match n {
        0 => format!("Nothing here matches {text}"),
        1 => format!("1 item matches {text}"),
        n => format!("{n} items match {text}"),
    });
}
