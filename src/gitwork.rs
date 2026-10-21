//! What Maia changes in a work tree when asked: staging and unstaging files, hunks and
//! lines, discarding, committing, branches and tags, checking out, cherry-picking,
//! reverting and resetting to commits, merging a branch, and the stash. Everything runs on a worker,
//! through `gitstatus::guard`, and nothing here ever pushes.
//!
//! Diffs come from the plumbing commands (`diff-files`, `diff-index`), which ignore the
//! user's diff settings and run no textconv or external diff, so a hunk read here
//! applies back with `git apply` as it was shown. A repository whose own config defines
//! content filters is only read: the guard empties those filters, and staging or
//! checking out through emptied ones would store or write the wrong bytes.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::gitlog::{Change, Spans};

/// The work tree's changes: the two lists a commit is made from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Work {
    /// In the index, against HEAD. Letters as git gives them (M, T, A, D, R, C).
    pub staged: Vec<Change>,
    /// In the files, against the index; `?` is untracked and `U` unmerged.
    pub unstaged: Vec<Change>,
    /// The commit HEAD is on; None before the first one.
    pub head: Option<String>,
    /// A merge, rebase, cherry-pick or revert stopped part way.
    pub doing: Option<crate::gitsync::Doing>,
}

impl Work {
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}

/// The empty tree's name in this repository (its hash depends on the object format).
fn empty_tree(root: &Path) -> Option<String> {
    let out = read(root, &["hash-object", "-t", "tree", "/dev/null"], 4096)?;
    Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|s| !s.is_empty())
}

/// What the index is compared with: HEAD, or nothing before the first commit.
pub fn base(root: &Path, head: Option<&str>) -> Option<String> {
    match head {
        Some(_) => Some("HEAD".into()),
        None => empty_tree(root),
    }
}

/// A read through the history's own git (guarded, never taking the index lock): the
/// output when git succeeds, at most `limit` bytes.
fn read(root: &Path, args: &[&str], limit: u64) -> Option<Vec<u8>> {
    let mut c = crate::gitlog::git(root);
    c.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    crate::media::output_capped(c, Duration::from_secs(30), limit, &|| false)
}

/// `git status --porcelain=v2 -z --branch` as the two lists.
pub fn parse_status(out: &[u8]) -> Work {
    let mut w = Work::default();
    let mut it = out.split(|&b| b == 0).map(|r| String::from_utf8_lossy(r).into_owned());
    while let Some(rec) = it.next() {
        if let Some(oid) = rec.strip_prefix("# branch.oid ") {
            w.head = (oid != "(initial)").then(|| oid.to_string());
            continue;
        }
        let kind = rec.as_bytes().first().copied().unwrap_or(0);
        let fields = match kind {
            b'1' => 9,
            b'2' => 10,
            b'u' => 11,
            b'?' => {
                w.unstaged.push(Change { status: b'?', path: rec[2..].to_string(), from: None, lines: None });
                continue;
            }
            _ => continue,
        };
        let f: Vec<&str> = rec.splitn(fields, ' ').collect();
        let (Some(xy), Some(path)) = (f.get(1), f.get(fields - 1)) else { continue };
        let (x, y) = (xy.as_bytes()[0], xy.as_bytes().get(1).copied().unwrap_or(b'.'));
        let from = if kind == b'2' { it.next() } else { None };
        if kind == b'u' {
            w.unstaged.push(Change { status: b'U', path: path.to_string(), from: None, lines: None });
            continue;
        }
        if x != b'.' {
            w.staged.push(Change { status: x, path: path.to_string(), from: from.clone(), lines: Some((0, 0)) });
        }
        if y != b'.' {
            w.unstaged.push(Change { status: y, path: path.to_string(), from: None, lines: Some((0, 0)) });
        }
    }
    w
}

/// Lines in an untracked file, or None when it is binary, too large or a folder.
fn count_lines(p: &Path) -> Option<(u32, u32)> {
    let m = std::fs::symlink_metadata(p).ok()?;
    if !m.is_file() || m.len() > 4 << 20 {
        return None;
    }
    let mut buf = vec![];
    std::fs::File::open(p).ok()?.take(4 << 20).read_to_end(&mut buf).ok()?;
    let sniff = &buf[..buf.len().min(8192)];
    if sniff.contains(&0) {
        return None;
    }
    let n = buf.iter().filter(|&&b| b == b'\n').count() + usize::from(buf.last().is_some_and(|&b| b != b'\n'));
    Some((n as u32, 0))
}

/// The work tree's changes with their line counts, or None outside one (or without git).
pub fn work(root: &Path) -> Option<Work> {
    let st = read(root, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=normal", "--ignored=no"], 64 << 20)?;
    let mut w = parse_status(&st);
    let base = base(root, w.head.as_deref())?;
    if !w.unstaged.is_empty() {
        let n = read(root, &["diff-files", "--numstat", "-z"], 16 << 20).unwrap_or_default();
        let (mut tracked, untracked): (Vec<Change>, Vec<Change>) = w.unstaged.drain(..).partition(|c| c.status != b'?');
        crate::gitlog::join_numstat(&mut tracked, &crate::gitlog::parse_numstat(&n));
        // Untracked files are counted by their lines: a few hundred, 64 MB read at most.
        let mut untracked = untracked;
        let mut budget: u64 = 64 << 20;
        for (k, c) in untracked.iter_mut().enumerate() {
            let size = std::fs::symlink_metadata(root.join(&c.path)).map_or(0, |m| m.len());
            c.lines = if k < 400 && !c.path.ends_with('/') && size <= budget { count_lines(&root.join(&c.path)) } else { None };
            budget = budget.saturating_sub(size.min(4 << 20));
        }
        tracked.extend(untracked);
        w.unstaged = tracked;
    }
    if !w.staged.is_empty() {
        let n = read(root, &["diff-index", "--cached", "--numstat", "-z", "-M", &base, "--"], 16 << 20).unwrap_or_default();
        crate::gitlog::join_numstat(&mut w.staged, &crate::gitlog::parse_numstat(&n));
    }
    w.doing = crate::gitsync::doing(root);
    Some(w)
}

/// One hunk of a file's change: its `@@` line, its lines as shown, and the bytes that
/// apply it on its own (the file's header and the hunk).
pub struct Hunk {
    pub head: String,
    pub lines: Vec<String>,
    pub spans: Option<Spans>,
    patch: Vec<u8>,
}

impl Hunk {
    /// The patch for this hunk alone, for `git apply`; None when it can't be applied
    /// on its own (a combined diff, a cut one, an untracked file).
    pub fn patch(&self) -> Option<&[u8]> {
        (!self.patch.is_empty()).then_some(&self.patch[..])
    }

    /// Whether some of its lines can go in or out alone: not for a file added or
    /// deleted whole, which only goes as a whole.
    pub fn splits(&self) -> bool {
        let p = &self.patch[..self.patch.windows(3).position(|w| w == b"\n@@").unwrap_or(0)];
        !p.is_empty() && !p.windows(10).any(|w| w == b"\nnew file " || w == b"\ndeleted f")
    }

    /// The patch for the `picked` lines alone (by their place in `lines`), to stage, or
    /// with `staged` to unstage. What is left out stays as the index has it: an
    /// unpicked line the index holds becomes context, one it doesn't is dropped. None
    /// when no change is picked.
    pub fn lines_patch(&self, picked: &[bool], staged: bool) -> Option<Vec<u8>> {
        if !self.splits() {
            return None;
        }
        let at = self.patch.windows(3).position(|w| w == b"\n@@")? + 1;
        let (header, hunk) = self.patch.split_at(at);
        let mut it = hunk.split_inclusive(|&b| b == b'\n');
        let head = std::str::from_utf8(it.next()?).ok()?;
        let (old, rest) = head.strip_prefix("@@ -")?.split_once(" +")?;
        let (new, tail) = rest.split_once(" @@")?;
        let start = |s: &str| s.split(',').next()?.parse::<u32>().ok();
        let (os, ns) = (start(old)?, start(new)?);
        // Unstaging takes the patch backwards: the index is its new side.
        let held = if staged { b'+' } else { b'-' };
        let (mut body, mut o, mut n, mut any, mut kept) = (vec![], 0, 0, false, true);
        for (i, l) in it.enumerate() {
            match l[0] {
                // "No newline at end of file" goes with the line before it.
                b'\\' if kept => body.extend_from_slice(l),
                b'\\' => {}
                c @ (b'+' | b'-') if picked.get(i).copied().unwrap_or(false) => {
                    any = true;
                    kept = true;
                    body.extend_from_slice(l);
                    if c == b'+' { n += 1 } else { o += 1 }
                }
                c @ (b'+' | b'-') => {
                    kept = c == held;
                    if kept {
                        body.push(b' ');
                        body.extend_from_slice(&l[1..]);
                        (o, n) = (o + 1, n + 1);
                    }
                }
                _ => {
                    kept = true;
                    body.extend_from_slice(l);
                    (o, n) = (o + 1, n + 1);
                }
            }
        }
        any.then(|| [header, format!("@@ -{os},{o} +{ns},{n} @@{tail}").as_bytes(), &body].concat())
    }
}

/// A changed file's diff in the work tree, hunk by hunk.
#[derive(Default)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    /// Why there are no lines to show (binary, a folder, a mode change).
    pub note: Option<&'static str>,
    /// Cut at 1 MB.
    pub cut: bool,
    /// An unmerged file's conflicts, shown in place of its diff.
    pub conflicts: Vec<crate::gitsync::Conflict>,
}

impl FileDiff {
    pub fn lines(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

/// Split a plumbing diff into hunks, each carrying the header so it applies alone.
pub fn split_hunks(out: &[u8], path: &str, applies: bool) -> FileDiff {
    let mut d = FileDiff::default();
    let lines: Vec<&[u8]> = out.split_inclusive(|&b| b == b'\n').collect();
    let first = lines.iter().position(|l| l.starts_with(b"@@"));
    let header: Vec<u8> = lines[..first.unwrap_or(lines.len())].concat();
    if header.windows(13).any(|w| w == b"Binary files ") || header.windows(12).any(|w| w == b"GIT binary p") {
        d.note = Some("Binary file");
        return d;
    }
    let Some(first) = first else {
        if !out.is_empty() {
            d.note = Some("No text changes");
        }
        return d;
    };
    // A combined diff (an unmerged file) shows, but its hunks don't apply alone.
    let applies = applies && !header.starts_with(b"diff --cc");
    let mut at = first;
    while at < lines.len() {
        let end = (at + 1..lines.len()).find(|&j| lines[j].starts_with(b"@@")).unwrap_or(lines.len());
        let text = |l: &[u8]| crate::gitlog::diff_lines(l).into_iter().next().unwrap_or_default();
        let head = text(lines[at]);
        let shown: Vec<String> = lines[at + 1..end].iter().map(|l| {
            let s = String::from_utf8_lossy(l);
            let s = s.trim_end_matches('\n');
            if s.len() > 2000 { format!("{}…", &s[..s.floor_char_boundary(2000)]) } else { s.replace('\t', "    ") }
        }).collect();
        let mut patch = if applies { [&header[..], &lines[at..end].concat()].concat() } else { vec![] };
        if patch.last().is_some_and(|&b| b != b'\n') {
            patch.push(b'\n');
        }
        d.hunks.push(Hunk { head, lines: shown, spans: None, patch });
        at = end;
    }
    highlight(&mut d, path);
    d
}

fn highlight(d: &mut FileDiff, path: &str) {
    let Some(lang) = crate::syntax::lang_for(path) else { return };
    for h in &mut d.hunks {
        let code: Vec<String> = h.lines.iter().map(|l| l.get(1..).unwrap_or("").to_string()).collect();
        h.spans = Some(crate::syntax::highlight(lang, &code, 20_000).into_iter().map(|s| s.into_iter().map(|(a, b, k)| (a + 1, b + 1, k)).collect()).collect());
    }
}

/// A file's change as staged (index against `base`) or not (files against the index).
pub fn file_diff(root: &Path, f: &Change, staged: bool, base: &str) -> FileDiff {
    if f.status == b'?' {
        return untracked_diff(&root.join(&f.path), &f.path);
    }
    if f.status == b'U' && !staged {
        let mut d = FileDiff::default();
        match std::fs::read_to_string(root.join(&f.path)) {
            Ok(t) => {
                d.conflicts = crate::gitsync::conflicts(&t);
                if d.conflicts.is_empty() {
                    d.note = Some("No conflicts left: stage it to mark it resolved");
                }
            }
            Err(_) => d.note = Some("Can't read this file as text"),
        }
        return d;
    }
    let mut a: Vec<&str> = if staged { vec!["diff-index", "-p", "--cached", "-M", "--no-color", base, "--"] } else { vec!["diff-files", "-p", "--no-color", "--"] };
    a.extend(f.from.as_deref());
    a.push(&f.path);
    let mut c = crate::gitlog::git(root);
    c.args(&a).stdin(Stdio::null()).stderr(Stdio::null());
    const CAP: u64 = 1 << 20;
    let out = crate::media::output_capped(c, Duration::from_secs(30), CAP, &|| false);
    let cut = out.is_none();
    let out = match out {
        Some(o) => o,
        // Over the cap: read again, cut, for showing only.
        None => {
            let mut c = crate::gitlog::git(root);
            c.args(&a).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
            let mut v = vec![];
            if let Ok(mut ch) = c.spawn() {
                if let Some(o) = ch.stdout.take() {
                    let _ = o.take(CAP).read_to_end(&mut v);
                }
                let _ = ch.kill();
                let _ = ch.wait();
            }
            v
        }
    };
    // A renamed file's hunks would carry the rename with them: whole files only.
    let mut d = split_hunks(&out, &f.path, !cut && f.from.is_none());
    d.cut = cut;
    if d.hunks.is_empty() && d.note.is_none() {
        d.note = Some(if f.status == b'T' { "Type changed" } else { "No text changes" });
    }
    d
}

/// An untracked file as one added hunk.
fn untracked_diff(p: &Path, path: &str) -> FileDiff {
    let mut d = FileDiff::default();
    if path.ends_with('/') {
        d.note = Some("Untracked folder");
        return d;
    }
    let mut buf = vec![];
    let Ok(f) = std::fs::File::open(p) else {
        d.note = Some("Can't read this file");
        return d;
    };
    let _ = f.take((1 << 20) + 1).read_to_end(&mut buf);
    d.cut = buf.len() > 1 << 20;
    buf.truncate(1 << 20);
    let Some(lines) = crate::inspector::text_lines(&buf) else {
        d.note = Some("Binary file");
        return d;
    };
    if lines.is_empty() {
        d.note = Some("Empty file");
        return d;
    }
    let head = format!("@@ -0,0 +1,{} @@", lines.len());
    d.hunks.push(Hunk { head, lines: lines.into_iter().map(|l| format!("+{l}")).collect(), spans: None, patch: vec![] });
    highlight(&mut d, path);
    d
}

/// The last commit's message, as summary and body (to amend it).
pub fn head_message(root: &Path) -> Option<(String, String)> {
    let out = read(root, &["log", "-1", "--format=%B", "HEAD", "--"], 256 << 10)?;
    let s = String::from_utf8_lossy(&out).trim_end().to_string();
    let (a, b) = s.split_once('\n').unwrap_or((&s, ""));
    Some((a.trim().to_string(), b.trim_start_matches('\n').to_string()))
}

/// A local branch, and how it stands against its upstream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Branch {
    pub name: String,
    pub upstream: String,
    pub ahead: u32,
    pub behind: u32,
    pub current: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stash {
    /// `stash@{n}`, and the commit it names when read.
    pub sel: String,
    pub hash: String,
    pub subject: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Its branch, or the short commit it is detached at.
    pub branch: String,
}

/// What the branch menu lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refs {
    pub local: Vec<Branch>,
    /// Remote branches no local branch tracks.
    pub remote: Vec<String>,
    pub stashes: Vec<Stash>,
    /// The other work trees of this repository.
    pub worktrees: Vec<Worktree>,
    /// The remotes, by name: with none there is nothing to fetch, pull or push.
    pub remotes: Vec<String>,
}

/// `for-each-ref` records: refname, upstream, track and HEAD, NUL between fields.
pub fn parse_refs(out: &[u8]) -> (Vec<Branch>, Vec<String>) {
    let (mut local, mut remote) = (vec![], vec![]);
    for line in out.split(|&b| b == b'\n') {
        let s = String::from_utf8_lossy(line);
        let f: Vec<&str> = s.split('\0').collect();
        let [refname, upstream, track, head] = f[..] else { continue };
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            let mut b = Branch { name: name.into(), upstream: upstream.into(), current: head == "*", ..Default::default() };
            for part in track.split(", ") {
                match part.split_once(' ') {
                    Some(("ahead", n)) => b.ahead = n.parse().unwrap_or(0),
                    Some(("behind", n)) => b.behind = n.parse().unwrap_or(0),
                    _ => {}
                }
            }
            local.push(b);
        } else if let Some(r) = refname.strip_prefix("refs/remotes/") {
            if !r.ends_with("/HEAD") && r.contains('/') {
                remote.push(r.to_string());
            }
        }
    }
    remote.retain(|r| !local.iter().any(|b: &Branch| b.upstream == *r));
    (local, remote)
}

/// `git worktree list --porcelain -z`, without the one at `root`.
pub fn parse_worktrees(out: &[u8], root: &Path) -> Vec<Worktree> {
    let mut v = vec![];
    let mut cur: Option<Worktree> = None;
    for f in out.split(|&b| b == 0) {
        let s = String::from_utf8_lossy(f);
        if let Some(p) = s.strip_prefix("worktree ") {
            v.extend(cur.take());
            cur = Some(Worktree { path: PathBuf::from(p), branch: String::new() });
        } else if let Some(c) = cur.as_mut() {
            if let Some(b) = s.strip_prefix("branch ") {
                c.branch = b.trim_start_matches("refs/heads/").to_string();
            } else if let Some(h) = s.strip_prefix("HEAD ") {
                if c.branch.is_empty() {
                    c.branch = h[..h.len().min(7)].to_string();
                }
            } else if s == "bare" {
                cur = None;
            }
        }
    }
    v.extend(cur);
    let same = |p: &Path| p == root || std::fs::canonicalize(p).ok() == std::fs::canonicalize(root).ok();
    v.retain(|w| !same(&w.path));
    v
}

pub fn refs(root: &Path) -> Refs {
    let out = read(root, &["for-each-ref", "--sort=-committerdate", "--count=400", "--format=%(refname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)", "refs/heads", "refs/remotes"], 4 << 20).unwrap_or_default();
    let (local, remote) = parse_refs(&out);
    let st = read(root, &["stash", "list", "--format=%gd%x1f%H%x1f%s"], 1 << 20).unwrap_or_default();
    let stashes = String::from_utf8_lossy(&st)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\x1f');
            Some(Stash { sel: f.next()?.to_string(), hash: f.next()?.to_string(), subject: f.next().unwrap_or("").to_string() })
        })
        .collect();
    let wt = read(root, &["worktree", "list", "--porcelain", "-z"], 1 << 20).unwrap_or_default();
    let rm = read(root, &["remote"], 1 << 16).unwrap_or_default();
    let remotes = String::from_utf8_lossy(&rm).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    Refs { local, remote, stashes, worktrees: parse_worktrees(&wt, root), remotes }
}

/// Something to change in a work tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Files to stage (a deletion and an untracked folder included).
    Stage(Vec<String>),
    Unstage(Vec<String>),
    StageAll,
    UnstageAll,
    /// One hunk into the index, or (`staged`) out of it again.
    Hunk { patch: Vec<u8>, staged: bool },
    /// `n` lines of a hunk, the same way (see `Hunk::lines_patch`).
    Lines { patch: Vec<u8>, staged: bool, n: usize },
    /// A file's unstaged changes, kept in the stash (an untracked file goes to the
    /// trash).
    Discard(Change),
    Commit { summary: String, body: String, amend: bool },
    Switch(String),
    /// A new branch from HEAD, switched to.
    Create(String),
    /// A local branch for a remote one, tracking it.
    Track(String),
    StashApply(Stash),
    StashPop(Stash),
    StashDrop(Stash),
    Fetch,
    /// Pull: fast-forward only (None), else merge (false) or rebase (true).
    Pull(Option<bool>),
    /// Push, or publish a branch with no upstream; `true` replaces what the remote has.
    Push(bool),
    /// One conflict in a file settled.
    Settle { path: String, at: usize, side: crate::gitsync::Side },
    /// Finish what is in progress, give up on it, or skip the commit it stopped at.
    Continue,
    Abort,
    Skip,
    /// A commit checked out on its own: HEAD on no branch.
    Checkout(String),
    /// A branch at a commit, switched to when asked.
    Branch { name: String, at: String, switch: bool },
    /// A tag at a commit, annotated when it has a message.
    Tag { name: String, at: String, message: String },
    /// A commit's change made again on HEAD, or undone by a new commit; a merge's
    /// change is the one against its first parent.
    CherryPick { hash: String, merge: bool },
    Revert { hash: String, merge: bool },
    /// The branch HEAD is on moved to a commit; `hard` resets the files too, once what
    /// isn't committed is in the stash.
    Reset { to: String, hard: bool },
    /// `force` deletes a branch whose commits are on no other.
    DeleteBranch { name: String, force: bool },
    RenameBranch { from: String, to: String },
    DeleteTag(String),
    /// A branch (local or remote) merged into the one HEAD is on.
    Merge(String),
}

impl Op {
    /// Whether the op reads or writes the files themselves (through content filters).
    fn filtered(&self) -> bool {
        matches!(
            self,
            Op::Stage(_) | Op::StageAll | Op::Hunk { staged: false, .. } | Op::Lines { staged: false, .. } | Op::Discard(_) | Op::Switch(_) | Op::Track(_) | Op::StashApply(_) | Op::StashPop(_) | Op::Pull(_) | Op::Continue | Op::Abort | Op::Skip
                | Op::Checkout(_) | Op::Branch { switch: true, .. } | Op::CherryPick { .. } | Op::Revert { .. } | Op::Reset { hard: true, .. } | Op::Merge(_)
        )
    }

    /// Whether, done with result `r`, it moved HEAD, a branch or a tag: the history is
    /// read again. A refused change moved nothing, unless it went on to more conflicts.
    pub fn moved(&self, r: &Result<String, String>) -> bool {
        let refs = !matches!(self, Op::Stage(_) | Op::Unstage(_) | Op::StageAll | Op::UnstageAll | Op::Hunk { .. } | Op::Lines { .. } | Op::Discard(_) | Op::StashApply(_) | Op::StashPop(_) | Op::StashDrop(_) | Op::Settle { .. });
        refs && r.as_ref().map_or_else(|e| e == crate::gitsync::CONFLICTS, |_| true)
    }
}

pub const FILTERED: &str = "This repository's own settings filter its files, which Maia doesn't run. Use a terminal for this one.";

/// A branch `git branch -d` kept: its commits are on no other branch. The app asks
/// whether to delete it anyway.
pub const UNMERGED: &str = "\u{1}unmerged";

/// git for a change: guarded, never prompting, never opening an editor.
pub(crate) fn exec(root: &Path, args: &[&str], input: Option<&[u8]>, timeout: Duration) -> Result<String, String> {
    let mut c = crate::media::tool("git");
    c.arg("--literal-pathspecs").args(crate::gitstatus::guard(root)).arg("-C").arg(root).args(["-c", "core.quotePath=false", "-c", "advice.detachedHead=false"]).args(args);
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", ":").env("GIT_SEQUENCE_EDITOR", ":").env("GIT_MERGE_AUTOEDIT", "no");
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("Couldn't run git: {e}"))?;
    if let (Some(data), Some(mut sin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = sin.write_all(&data);
        });
    }
    let pipe = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut v = vec![];
            if let Some(mut r) = r {
                let _ = r.by_ref().take(1 << 20).read_to_end(&mut v);
            }
            v
        })
    };
    let out = pipe(child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>));
    let err = pipe(child.stderr.take().map(|o| Box::new(o) as Box<dyn Read + Send>));
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    match status {
        Some(st) if st.success() => Ok(text(&out)),
        Some(_) => {
            // A hook may say why on either stream.
            let msg = [text(&err), text(&out)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            Err(plain(&msg))
        }
        None => Err("git took too long and was stopped".into()),
    }
}

/// git's message without its "error:" and "fatal:" prefixes and hint lines, at most
/// twelve lines.
pub fn plain(msg: &str) -> String {
    let lines: Vec<&str> = msg
        .lines()
        .filter(|l| !l.starts_with("hint:"))
        .map(|l| l.strip_prefix("error: ").or_else(|| l.strip_prefix("fatal: ")).unwrap_or(l).trim_end())
        .filter(|l| !l.is_empty())
        .collect();
    let mut s = lines.iter().take(12).copied().collect::<Vec<_>>().join("\n");
    if lines.len() > 12 {
        s.push_str("\n…");
    }
    if s.is_empty() { "git couldn't do it".into() } else { s }
}

fn short(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

/// The commit a stash selector names now: the stash list may have moved since it
/// was read.
fn still(root: &Path, s: &Stash) -> Result<(), String> {
    let now = read(root, &["rev-parse", "--verify", "-q", &s.sel], 4096).map(|o| String::from_utf8_lossy(&o).trim().to_string());
    if now.as_deref() == Some(s.hash.as_str()) { Ok(()) } else { Err("The stash list has changed. Open the menu again.".into()) }
}

/// A name git takes for a new branch or tag, trimmed, or why not.
fn ref_name<'a>(root: &Path, name: &'a str, tag: bool) -> Result<&'a str, String> {
    let (n, what) = (name.trim(), if tag { "tag" } else { "branch" });
    if n.is_empty() {
        return Err(format!("A {what} needs a name"));
    }
    let full = format!("refs/tags/{n}");
    let check: &[&str] = if tag { &["check-ref-format", &full] } else { &["check-ref-format", "--branch", n] };
    if n.starts_with('-') || exec(root, check, None, Duration::from_secs(30)).is_err() {
        return Err(format!("{n:?} can't be a {what} name"));
    }
    Ok(n)
}

fn short_hash(h: &str) -> &str {
    &h[..h.len().min(7)]
}

/// The commit git says a deleted branch or tag was at ("Deleted branch x (was 1a2b3c4).").
fn was(out: &str) -> &str {
    out.rsplit_once("(was ").and_then(|(_, r)| r.split(')').next()).unwrap_or("")
}

/// Why a cherry-pick or revert stopped: at conflicts the Changes page takes over; with
/// nothing to change, it is given up on and `empty` said.
fn stopped(root: &Path, e: String, d: crate::gitsync::Doing, empty: String) -> String {
    match crate::gitsync::doing(root) {
        Some(_) if crate::gitsync::unmerged(root) > 0 => crate::gitsync::CONFLICTS.into(),
        Some(now) if now == d => {
            let _ = crate::gitsync::abort(root);
            empty
        }
        _ => e,
    }
}

/// Make the change; Ok holds what to say about it, Err git's reason (plainly).
pub fn run(root: &Path, op: &Op) -> Result<String, String> {
    if op.filtered() && !crate::gitstatus::local_filters(root).is_empty() {
        return Err(FILTERED.into());
    }
    let quick = Duration::from_secs(120);
    let head = read(root, &["rev-parse", "--verify", "-q", "HEAD"], 4096).is_some();
    let with = |base: &[&str], paths: &[String]| -> Vec<String> { base.iter().map(|s| s.to_string()).chain(paths.iter().cloned()).collect() };
    let call = |args: Vec<String>, input: Option<&[u8]>, t: Duration| {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        exec(root, &a, input, t)
    };
    let one = |paths: &[String]| if paths.len() == 1 { short(&paths[0]).to_string() } else { format!("{} files", paths.len()) };
    match op {
        // A file in conflict staged with its markers in would commit them.
        Op::Stage(paths) if paths.iter().any(|p| crate::gitsync::has_markers(&root.join(p)) && read(root, &["ls-files", "-u", "--", p], 4096).is_some_and(|o| !o.is_empty())) => {
            Err("It still has conflict markers: settle them first.".into())
        }
        Op::Stage(paths) => call(with(&["add", "-A", "--"], paths), None, quick).map(|_| format!("Staged {}", one(paths))),
        Op::Unstage(paths) if head => call(with(&["restore", "-q", "--staged", "--"], paths), None, quick).map(|_| format!("Unstaged {}", one(paths))),
        Op::Unstage(paths) => call(with(&["rm", "-q", "-r", "-f", "--cached", "--"], paths), None, quick).map(|_| format!("Unstaged {}", one(paths))),
        Op::StageAll => exec(root, &["add", "-A"], None, quick).map(|_| "Staged every change".into()),
        Op::UnstageAll if head => exec(root, &["reset", "-q"], None, quick).map(|_| "Unstaged everything".into()),
        Op::UnstageAll => exec(root, &["rm", "-q", "-r", "-f", "--cached", "--", "."], None, quick).map(|_| "Unstaged everything".into()),
        Op::Hunk { patch, staged } | Op::Lines { patch, staged, .. } => {
            let mut a = vec!["apply", "--cached", "--whitespace=nowarn"];
            if *staged {
                a.push("-R");
            }
            a.push("-");
            exec(root, &a, Some(patch), quick)?;
            let what = match op {
                Op::Lines { n: 1, .. } => "1 line".into(),
                Op::Lines { n, .. } => format!("{n} lines"),
                _ => "the hunk".into(),
            };
            Ok(format!("{} {what}", if *staged { "Unstaged" } else { "Staged" }))
        }
        Op::Discard(f) if f.status == b'?' => {
            let p = root.join(f.path.trim_end_matches('/'));
            crate::trashcan::delete(&p).map(|_| format!("Moved {} to the trash", short(&f.path))).map_err(|e| crate::fsx::plain_error(&e.to_string()).to_string())
        }
        Op::Discard(f) => {
            let msg = format!("Discarded in Maia: {}", f.path);
            let mut a = vec!["stash".to_string(), "push".into(), "-q".into(), "--keep-index".into(), "-m".into(), msg, "--".into(), f.path.clone()];
            a.extend(f.from.clone());
            call(a, None, quick).map(|_| format!("Discarded the changes to {} · kept in the stash", short(&f.path)))
        }
        Op::Commit { summary, body, amend } => {
            if summary.trim().is_empty() {
                return Err("Write a summary first".into());
            }
            if !*amend {
                let base = base(root, head.then_some("HEAD")).ok_or("git couldn't read the repository")?;
                let merging = read(root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], 4096).is_some();
                if !merging && exec(root, &["diff-index", "--cached", "--quiet", &base, "--"], None, quick).is_ok() {
                    return Err("Nothing is staged".into());
                }
            }
            let msg = if body.trim().is_empty() { format!("{}\n", summary.trim()) } else { format!("{}\n\n{}\n", summary.trim(), body.trim_end()) };
            let mut a = vec!["commit", "-q", "-F", "-"];
            if *amend {
                a.push("--amend");
            }
            // Hooks can take their time (a test suite); they run on this worker.
            exec(root, &a, Some(msg.as_bytes()), Duration::from_secs(600))?;
            let h = read(root, &["log", "-1", "--format=%h", "HEAD", "--"], 4096).map(|o| String::from_utf8_lossy(&o).trim().to_string()).unwrap_or_default();
            Ok(format!("{} {h}", if *amend { "Amended" } else { "Committed" }))
        }
        Op::Switch(b) => exec(root, &["switch", "-q", "--no-guess", b], None, quick).map(|_| format!("Switched to {b}")),
        Op::Create(b) => {
            let b = ref_name(root, b, false)?;
            exec(root, &["switch", "-q", "-c", b], None, quick).map(|_| format!("Created and switched to {b}"))
        }
        Op::Track(r) => exec(root, &["switch", "-q", "--track", r], None, quick).map(|_| format!("Switched to {}", r.split_once('/').map_or(r.as_str(), |x| x.1))),
        Op::StashApply(s) => {
            still(root, s)?;
            exec(root, &["stash", "apply", "-q", &s.sel], None, quick).map(|_| format!("Applied {}", s.sel))
        }
        Op::StashPop(s) => {
            still(root, s)?;
            exec(root, &["stash", "pop", "-q", &s.sel], None, quick).map(|_| format!("Applied and dropped {}", s.sel))
        }
        Op::StashDrop(s) => {
            still(root, s)?;
            exec(root, &["stash", "drop", "-q", &s.sel], None, quick).map(|_| format!("Dropped {}", s.sel))
        }
        Op::Fetch => crate::gitsync::fetch(root),
        Op::Pull(how) => crate::gitsync::pull(root, *how),
        Op::Push(force) => crate::gitsync::push(root, *force),
        Op::Settle { path, at, side } => crate::gitsync::settle(root, path, *at, *side),
        Op::Continue => crate::gitsync::go_on(root),
        Op::Abort => crate::gitsync::abort(root),
        Op::Skip => crate::gitsync::skip(root),
        // git refuses when a change in the files would be overwritten, and says which.
        Op::Checkout(at) => exec(root, &["switch", "-q", "--detach", at], None, quick).map(|_| format!("Checked out {}, on no branch", short_hash(at))),
        Op::Branch { name, at, switch } => {
            let b = ref_name(root, name, false)?;
            if *switch {
                exec(root, &["switch", "-q", "-c", b, at], None, quick).map(|_| format!("Created {b} at {} and switched to it", short_hash(at)))
            } else {
                exec(root, &["branch", b, at], None, quick).map(|_| format!("Created {b} at {}", short_hash(at)))
            }
        }
        Op::Tag { name, at, message } => {
            let t = ref_name(root, name, true)?;
            let m = message.trim();
            if m.is_empty() { exec(root, &["tag", t, at], None, quick) } else { exec(root, &["tag", "-a", "-F", "-", t, at], Some(m.as_bytes()), quick) }.map(|_| format!("Tagged {} as {t}", short_hash(at)))
        }
        Op::CherryPick { hash, merge } => {
            let onto = crate::gitsync::branch(root).unwrap_or_else(|| "HEAD".into());
            let h = short_hash(hash);
            if exec(root, &["merge-base", "--is-ancestor", hash, "HEAD"], None, quick).is_ok() {
                return Err(format!("{h} is already on {onto}."));
            }
            let a: &[&str] = if *merge { &["cherry-pick", "-m", "1", hash] } else { &["cherry-pick", hash] };
            match exec(root, a, None, quick) {
                Ok(_) => Ok(format!("Cherry-picked {h} onto {onto}")),
                Err(e) => Err(stopped(root, e, crate::gitsync::Doing::CherryPick, format!("{onto} already has what {h} changes."))),
            }
        }
        Op::Revert { hash, merge } => {
            let h = short_hash(hash);
            let a: &[&str] = if *merge { &["revert", "--no-edit", "-m", "1", hash] } else { &["revert", "--no-edit", hash] };
            match exec(root, a, None, quick) {
                Ok(_) => Ok(format!("Reverted {h}")),
                Err(e) => Err(stopped(root, e, crate::gitsync::Doing::Revert, format!("What {h} changes is already undone."))),
            }
        }
        Op::Reset { to, hard } => {
            let b = crate::gitsync::branch(root).unwrap_or_else(|| "HEAD".into());
            let old = exec(root, &["rev-parse", "--short", "HEAD"], None, quick)?;
            // What isn't committed is kept in the stash: a hard reset would lose it.
            let stash = || read(root, &["rev-parse", "-q", "--verify", "refs/stash"], 4096);
            let before = stash();
            if *hard {
                exec(root, &["stash", "push", "-q", "-m", &format!("Before resetting {b} in Maia")], None, quick)?;
            }
            exec(root, &["reset", "-q", if *hard { "--hard" } else { "--mixed" }, to], None, quick)?;
            let kept = if stash() != before { ", your changes kept in the stash" } else { "" };
            Ok(format!("Reset {b} to {} · it was at {old}{kept}", short_hash(to)))
        }
        Op::DeleteBranch { name, force } => match exec(root, &["branch", if *force { "-D" } else { "-d" }, name], None, quick) {
            Err(e) if !force && e.contains("not fully merged") => Err(UNMERGED.into()),
            r => r.map(|out| format!("Deleted {name} · it was at {}", was(&out))),
        },
        Op::RenameBranch { from, to } => {
            let to = ref_name(root, to, false)?;
            exec(root, &["branch", "-m", from, to], None, quick).map(|_| format!("Renamed {from} to {to}"))
        }
        Op::DeleteTag(t) => exec(root, &["tag", "-d", t], None, quick).map(|out| format!("Deleted the tag {t} · it was at {}", was(&out))),
        Op::Merge(b) => crate::gitsync::merge(root, b),
    }
}

/// A change started from a menu: the result is said once it is done.
struct Pending {
    root: PathBuf,
    title: String,
    /// Said in the status bar while it runs ("Pulling…"), for the slow ones.
    doing: Option<&'static str>,
    slot: crate::gitlog::Slot<(Op, Result<String, String>)>,
}

thread_local! {
    static PENDING: std::cell::RefCell<Vec<Pending>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `op` on a worker; `title` heads its error ("Couldn't switch to docs").
pub fn start(ctx: &egui::Context, root: &Path, op: Op, title: String) {
    let r = root.to_path_buf();
    let doing = match &op {
        Op::Fetch => Some("Fetching…"),
        Op::Pull(_) => Some("Pulling…"),
        Op::Push(_) => Some("Pushing…"),
        Op::Merge(_) => Some("Merging…"),
        _ => None,
    };
    let slot = crate::gitlog::spawn(ctx, move || {
        let res = run(&r, &op);
        (op, res)
    });
    PENDING.with(|p| p.borrow_mut().push(Pending { root: root.to_path_buf(), title, doing, slot }));
}

/// What is being sent or got in the work tree at `root`, for the status bar.
pub fn busy(root: &Path) -> Option<&'static str> {
    PENDING.with(|p| p.borrow().iter().find(|x| x.root == root).and_then(|x| x.doing))
}

/// Changes started with `start` that have finished: (work tree, title, the change,
/// result).
pub fn finished() -> Vec<(PathBuf, String, Op, Result<String, String>)> {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let mut done = vec![];
        p.retain(|x| match x.slot.lock().ok().and_then(|mut s| s.take()) {
            Some((op, r)) => {
                done.push((x.root.clone(), x.title.clone(), op, r));
                false
            }
            None => true,
        });
        done
    })
}

/// The branch menu's lists, read on a worker.
pub fn refs_job(ctx: &egui::Context, root: &Path) -> Arc<Mutex<Option<Refs>>> {
    let r = root.to_path_buf();
    crate::gitlog::spawn(ctx, move || refs(&r))
}
