//! Sending and getting work: fetch, pull and push for the branch HEAD is on, merging
//! another branch into it, and the conflicts a merge, rebase, cherry-pick or revert stops
//! at, one at a time. git does the work on a worker and never asks for anything at a
//! terminal; what it says is put in plain words.

use std::path::Path;
use std::time::Duration;

/// A pull that can't fast-forward: both sides have moved on. The app asks whether to
/// merge or rebase.
pub const DIVERGED: &str = "\u{1}diverged";
/// A push the remote refused: it has commits this branch doesn't.
pub const BEHIND: &str = "\u{1}behind";
/// A merge, rebase, cherry-pick or revert that stopped at conflicts: the app opens the
/// Changes page.
pub const CONFLICTS: &str = "\u{1}conflicts";

/// Network work: slow links and large fetches get their time, a hang doesn't last.
const SLOW: Duration = Duration::from_secs(600);
const QUICK: Duration = Duration::from_secs(60);

fn exec(root: &Path, args: &[&str], t: Duration) -> Result<String, String> {
    crate::gitwork::exec(root, args, None, t)
}

fn out(root: &Path, args: &[&str]) -> Option<String> {
    exec(root, args, QUICK).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// What git said, said plainly when it is one of the usual troubles.
pub fn explain(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("terminal prompts disabled") || l.contains("could not read username") || l.contains("authentication failed") {
        "The remote wants you to sign in. Set up an SSH key or a credential helper for it, then try again.".into()
    } else if l.contains("permission denied (publickey") {
        "The remote turned down your SSH key.".into()
    } else if l.contains("host key verification failed") {
        "This computer doesn't know the remote's SSH host key yet. Connect to it once from a terminal to accept it.".into()
    } else if l.contains("could not resolve host") || l.contains("network is unreachable") || l.contains("connection timed out") || l.contains("failed to connect") {
        "Can't reach the remote. Check the connection.".into()
    } else if l.contains("repository not found") || l.contains("does not appear to be a git repository") {
        "The remote isn't there, or you can't see it.".into()
    } else if e.contains("timed out") {
        "git took too long and was stopped.".into()
    } else {
        e.trim().to_string()
    }
}

/// The branch HEAD is on, or None when it is detached.
pub(crate) fn branch(root: &Path) -> Option<String> {
    out(root, &["symbolic-ref", "-q", "--short", "HEAD"])
}

/// The branch HEAD's upstream ("origin/main"), if it has one.
pub fn upstream(root: &Path) -> Option<String> {
    out(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])
}

/// Commits HEAD has that its upstream lacks, and the other way round.
fn counts(root: &Path) -> Option<(u32, u32)> {
    let s = out(root, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut it = s.split_whitespace().filter_map(|n| n.parse().ok());
    Some((it.next()?, it.next()?))
}

fn commits(n: u32) -> String {
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// The remote a new branch goes to: `origin` when there is one, else the only one.
fn remote_for_new(root: &Path) -> Result<String, String> {
    let all = out(root, &["remote"]).unwrap_or_default();
    let names: Vec<&str> = all.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match names.as_slice() {
        [] => Err("This repository has no remote to send it to.".into()),
        n if n.contains(&"origin") => Ok("origin".into()),
        [one] => Ok(one.to_string()),
        _ => Err("There are several remotes and none is called origin: publish this branch from a terminal.".into()),
    }
}

pub fn fetch(root: &Path) -> Result<String, String> {
    exec(root, &["fetch", "--all", "--prune", "--quiet"], SLOW).map_err(|e| explain(&e))?;
    Ok(match counts(root) {
        Some((_, 0)) => "Fetched: up to date".into(),
        Some((_, b)) => format!("Fetched: {} to pull", commits(b)),
        None => "Fetched".into(),
    })
}

pub fn pull(root: &Path, how: Option<bool>) -> Result<String, String> {
    let b = branch(root).ok_or("HEAD isn't on a branch, so there is nothing to pull into.")?;
    let up = upstream(root).ok_or_else(|| format!("{b} doesn't track a remote branch yet. Push publishes it."))?;
    let before = out(root, &["rev-parse", "HEAD"]);
    let r = match how {
        None => exec(root, &["pull", "--ff-only", "--quiet"], SLOW),
        Some(false) => exec(root, &["pull", "--no-rebase", "--no-edit", "--quiet"], SLOW),
        Some(true) => exec(root, &["pull", "--rebase", "--quiet"], SLOW),
    };
    if let Err(e) = r {
        if doing(root).is_some() {
            return Err(CONFLICTS.into());
        }
        let l = e.to_lowercase();
        if how.is_none() && (l.contains("not possible to fast-forward") || l.contains("diverg")) {
            return Err(DIVERGED.into());
        }
        return Err(explain(&e));
    }
    let after = out(root, &["rev-parse", "HEAD"]);
    if before == after {
        return Ok("Already up to date".into());
    }
    let n = match (&before, &after) {
        (Some(a), Some(z)) => out(root, &["rev-list", "--count", &format!("{a}..{z}")]).and_then(|s| s.parse().ok()),
        _ => None,
    };
    Ok(match (how, n) {
        (Some(false), _) => format!("Merged {up}"),
        (Some(true), _) => format!("Rebased onto {up}"),
        (None, Some(n)) => format!("Pulled {}", commits(n)),
        (None, None) => "Pulled".into(),
    })
}

pub fn push(root: &Path, force: bool) -> Result<String, String> {
    let b = branch(root).ok_or("HEAD isn't on a branch, so there is nothing to push.")?;
    let Some(up) = upstream(root) else {
        let remote = remote_for_new(root)?;
        exec(root, &["push", "--quiet", "-u", &remote, "HEAD"], SLOW).map_err(|e| explain(&e))?;
        return Ok(format!("Published {b} to {remote}"));
    };
    let ahead = counts(root).map(|c| c.0);
    let args: &[&str] = if force { &["push", "--quiet", "--force-with-lease"] } else { &["push", "--quiet"] };
    if let Err(e) = exec(root, args, SLOW) {
        let l = e.to_lowercase();
        if !force && l.contains("rejected") && (l.contains("fetch first") || l.contains("non-fast-forward")) {
            return Err(BEHIND.into());
        }
        if force && l.contains("stale info") {
            return Err(format!("{up} changed since it was last fetched, so it wasn't replaced. Fetch and look first."));
        }
        return Err(explain(&e));
    }
    Ok(match (force, ahead) {
        (true, _) => format!("Pushed, replacing {up}"),
        (false, Some(0)) => "Nothing to push".into(),
        (false, Some(n)) => format!("Pushed {} to {up}", commits(n)),
        (false, None) => format!("Pushed to {up}"),
    })
}

/// The files git names, one to a line under its message, as "a.txt, b.txt and 3 more".
fn named(e: &str, after: &str) -> String {
    let files: Vec<&str> = e.lines().skip_while(|l| !l.contains(after)).skip(1).take_while(|l| l.starts_with(['\t', ' '])).map(str::trim).filter(|l| !l.is_empty()).collect();
    match files.as_slice() {
        [] => "some files".into(),
        [a] => (*a).into(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// A refused merge in plain words: what is in the way, and what to do about it.
pub fn merge_refusal(e: &str, what: &str, into: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("your local changes to the following files would be overwritten") {
        format!("Your uncommitted changes to {} would be overwritten by the merge. Commit or stash them first.", named(e, "would be overwritten"))
    } else if l.contains("untracked working tree files would be overwritten") {
        format!("The merge would overwrite {}, which git doesn't track. Move them away or commit them first.", named(e, "would be overwritten"))
    } else if l.contains("unrelated histories") {
        format!("{what} and {into} have no history in common, so git won't merge them.")
    } else if l.contains("not something we can merge") {
        format!("git doesn't know {what}.")
    } else if l.contains("you have not concluded your merge") || l.contains("merge_head exists") {
        "A merge is still in progress: continue or abort it on the Changes page first.".into()
    } else {
        explain(e)
    }
}

/// Merge `what` (a branch, remote or local) into the branch HEAD is on. A fast-forward
/// when git can: git's own `merge.ff` setting decides, as it would at a terminal; else a
/// merge commit with git's message, no editor opened. Conflicts are left to the Changes
/// page. What is said names the commit it was at, which Reset can go back to.
pub fn merge(root: &Path, what: &str) -> Result<String, String> {
    let into = branch(root).unwrap_or_else(|| "HEAD".into());
    if let Some(d) = doing(root) {
        return Err(format!("{} is still in progress: continue or abort it on the Changes page first.", d.label()));
    }
    let before = out(root, &["rev-parse", "HEAD"]).ok_or("There is no commit to merge into yet.")?;
    // Hooks may run, as for a commit.
    if let Err(e) = exec(root, &["merge", "--no-edit", "--quiet", what], Duration::from_secs(600)) {
        if doing(root) == Some(Doing::Merge) && unmerged(root) > 0 {
            return Err(CONFLICTS.into());
        }
        return Err(merge_refusal(&e, what, &into));
    }
    let after = out(root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let was = &before[..before.len().min(7)];
    if after == before {
        return Ok(format!("{into} already has everything on {what}"));
    }
    let theirs = out(root, &["rev-parse", &format!("{what}^{{commit}}")]).unwrap_or_default();
    Ok(if after == theirs { format!("Fast-forwarded {into} to {what} · it was at {was}") } else { format!("Merged {what} into {into} · it was at {was}") })
}

// ------------------------------------------------------------------ in the middle of one

/// What the work tree is in the middle of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Doing {
    pub fn label(self) -> &'static str {
        match self {
            Doing::Merge => "Merging",
            Doing::Rebase => "Rebasing",
            Doing::CherryPick => "Cherry-picking",
            Doing::Revert => "Reverting",
        }
    }
}

/// Read from the files git keeps while it is one of them.
pub fn doing(root: &Path) -> Option<Doing> {
    let gd = out(root, &["rev-parse", "--git-dir"])?;
    let gd = if Path::new(&gd).is_absolute() { std::path::PathBuf::from(gd) } else { root.join(gd) };
    if gd.join("rebase-merge").is_dir() || gd.join("rebase-apply").is_dir() {
        Some(Doing::Rebase)
    } else if gd.join("MERGE_HEAD").is_file() {
        Some(Doing::Merge)
    } else if gd.join("CHERRY_PICK_HEAD").is_file() {
        Some(Doing::CherryPick)
    } else if gd.join("REVERT_HEAD").is_file() {
        Some(Doing::Revert)
    } else {
        None
    }
}

pub(crate) fn unmerged(root: &Path) -> usize {
    out(root, &["diff", "--name-only", "--diff-filter=U"]).map_or(0, |s| s.lines().count())
}

/// Finish what was stopped, once nothing is left in conflict.
pub fn go_on(root: &Path) -> Result<String, String> {
    let d = doing(root).ok_or("Nothing is in progress.")?;
    if unmerged(root) > 0 {
        return Err("Some files are still in conflict.".into());
    }
    let r = match d {
        Doing::Rebase => exec(root, &["rebase", "--continue"], QUICK),
        Doing::Merge => exec(root, &["commit", "--no-edit", "-q"], Duration::from_secs(600)),
        Doing::CherryPick => exec(root, &["cherry-pick", "--continue"], QUICK),
        Doing::Revert => exec(root, &["revert", "--continue"], QUICK),
    };
    match r {
        Ok(_) if doing(root).is_some() => Err(CONFLICTS.into()),
        Ok(_) => Ok(match d {
            Doing::Merge => "Finished the merge",
            Doing::Rebase => "Finished the rebase",
            Doing::CherryPick => "Finished the cherry-pick",
            Doing::Revert => "Finished the revert",
        }
        .into()),
        Err(_) if doing(root).is_some() && unmerged(root) > 0 => Err(CONFLICTS.into()),
        Err(e) => Err(explain(&e)),
    }
}

/// The git command that carries it on, or gives it up.
fn command(d: Doing) -> &'static str {
    match d {
        Doing::Rebase => "rebase",
        Doing::Merge => "merge",
        Doing::CherryPick => "cherry-pick",
        Doing::Revert => "revert",
    }
}

/// Give up on it: the work tree goes back to where it was before.
pub fn abort(root: &Path) -> Result<String, String> {
    let d = doing(root).ok_or("Nothing is in progress.")?;
    exec(root, &[command(d), "--abort"], QUICK).map_err(|e| explain(&e))?;
    Ok(format!("Stopped {} and went back", d.label().to_lowercase()))
}

/// Leave out the commit a rebase, cherry-pick or revert stopped at.
pub fn skip(root: &Path) -> Result<String, String> {
    let d = doing(root).ok_or("Nothing is in progress.")?;
    if d == Doing::Merge {
        return Err("A merge has no commit to skip.".into());
    }
    exec(root, &[command(d), "--skip"], QUICK).map_err(|e| explain(&e))?;
    if doing(root).is_some() && unmerged(root) > 0 {
        return Err(CONFLICTS.into());
    }
    Ok("Skipped that commit".into())
}

// ------------------------------------------------------------------ one file's conflicts

/// One place two sides of a file disagree, between git's markers.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// The names git writes after `<<<<<<<` and `>>>>>>>`.
    pub ours: String,
    pub theirs: String,
    pub ours_lines: Vec<String>,
    pub theirs_lines: Vec<String>,
    /// Where it starts (from 1).
    pub line: usize,
}

/// Which side to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
    Both,
}

fn marker<'a>(l: &'a str, m: &str) -> Option<&'a str> {
    let rest = l.strip_prefix(m)?;
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

/// The conflicts in a file's text, in order. A base section (`|||||||`, from the
/// diff3 style) is read past; the two sides are what is kept.
pub fn conflicts(text: &str) -> Vec<Conflict> {
    let mut v = vec![];
    let mut cur: Option<(Conflict, u8)> = None;
    for (i, l) in text.lines().enumerate() {
        match &mut cur {
            None => {
                if let Some(name) = marker(l, "<<<<<<<") {
                    cur = Some((Conflict { ours: name.into(), theirs: String::new(), ours_lines: vec![], theirs_lines: vec![], line: i + 1 }, 0));
                }
            }
            Some((c, part)) => {
                if *part < 2 && marker(l, "|||||||").is_some() {
                    *part = 1;
                } else if *part < 2 && l == "=======" {
                    *part = 2;
                } else if let (2, Some(name)) = (*part, marker(l, ">>>>>>>")) {
                    c.theirs = name.into();
                    v.push(cur.take().unwrap().0);
                } else if *part == 0 {
                    c.ours_lines.push(l.to_string());
                } else if *part == 2 {
                    c.theirs_lines.push(l.to_string());
                }
            }
        }
    }
    v
}

/// The text with conflict `at` (from 0) settled by keeping `side`; the others stay.
pub fn resolve(text: &str, at: usize, side: Side) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut k = 0;
    let mut part: Option<u8> = None;
    let mut ours: Vec<&str> = vec![];
    let mut theirs: Vec<&str> = vec![];
    let mut raw: Vec<&str> = vec![];
    let mut found = false;
    for l in text.split_inclusive('\n') {
        let bare = l.trim_end_matches('\n').trim_end_matches('\r');
        match part {
            None if marker(bare, "<<<<<<<").is_some() => {
                part = Some(0);
                raw = vec![l];
                ours.clear();
                theirs.clear();
            }
            None => out.push_str(l),
            Some(p) => {
                raw.push(l);
                if p < 2 && marker(bare, "|||||||").is_some() {
                    part = Some(1);
                } else if p < 2 && bare == "=======" {
                    part = Some(2);
                } else if p == 2 && marker(bare, ">>>>>>>").is_some() {
                    if k == at {
                        found = true;
                        let keep: Vec<&str> = match side {
                            Side::Ours => ours.clone(),
                            Side::Theirs => theirs.clone(),
                            Side::Both => ours.iter().chain(theirs.iter()).copied().collect(),
                        };
                        keep.iter().for_each(|x| out.push_str(x));
                    } else {
                        raw.iter().for_each(|x| out.push_str(x));
                    }
                    k += 1;
                    part = None;
                } else if p == 0 {
                    ours.push(l);
                } else if p == 2 {
                    theirs.push(l);
                }
            }
        }
    }
    // A conflict never closed is left as it was.
    if part.is_some() {
        raw.iter().for_each(|x| out.push_str(x));
    }
    found.then_some(out)
}

/// Settle one conflict in `path` (from the work tree's top); with none left, the file
/// is marked resolved.
pub fn settle(root: &Path, path: &str, at: usize, side: Side) -> Result<String, String> {
    let p = root.join(path);
    let text = std::fs::read_to_string(&p).map_err(|e| format!("Couldn't read {path}: {e}"))?;
    let new = resolve(&text, at, side).ok_or("That conflict is no longer in the file.")?;
    std::fs::write(&p, &new).map_err(|e| format!("Couldn't write {path}: {e}"))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    if conflicts(&new).is_empty() {
        exec(root, &["add", "--", path], QUICK).map_err(|e| explain(&e))?;
        return Ok(format!("Resolved {name}"));
    }
    Ok(format!("{} left in {name}", conflicts(&new).len()))
}

/// Whether a file still holds conflict markers (staging it then would commit them).
pub fn has_markers(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|t| !conflicts(&t).is_empty())
}
