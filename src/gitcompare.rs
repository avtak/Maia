//! Two branches or commits compared: the commits each has that the other doesn't, and
//! what differs in the files, either since the two split (what the newer side adds, as
//! `git diff A...B` shows it) or between the two directly (`git diff A B`). The files
//! and their diffs are the commit view's, with the old side standing in for the parent.

use std::path::Path;

use crate::gitlog::Commit;

/// Commits listed per side; the count above them says how many there are in all.
pub const SHOWN: usize = 200;

pub struct Sides {
    /// The old side and the new, as commits.
    pub hashes: [String; 2],
    /// Where they split; None when they share no history.
    pub base: Option<String>,
    /// The commits only on each side, newest first, and how many there are.
    pub only: [Vec<Commit>; 2],
    pub counts: [usize; 2],
    /// The two were swapped so the older is the old side.
    pub swapped: bool,
}

fn said(root: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&crate::gitlog::run(root, args, 1 << 16).0).trim().to_string()
}

fn commit_of(root: &Path, rev: &str) -> Option<String> {
    Some(said(root, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])).filter(|h| h.len() >= 7 && !h.contains(char::is_whitespace))
}

/// The commits on `yes` that `no` lacks, newest first.
fn only(root: &Path, yes: &str, no: &str) -> Vec<Commit> {
    let n = SHOWN.to_string();
    let not = format!("^{no}");
    let out = crate::gitlog::run(root, &["log", "--topo-order", "-z", "--no-color", "--decorate=full", crate::gitlog::FORMAT, "-n", &n, yes, &not, "--"], 16 << 20).0;
    crate::gitlog::read_page(&mut &out[..], SHOWN)
}

/// `old` and `new` read as commits, compared. With `order`, two on one line of history are
/// put older first, whichever was named first.
pub fn sides(root: &Path, old: &str, new: &str, order: bool) -> Result<Sides, String> {
    let a = commit_of(root, old).ok_or_else(|| format!("git doesn't know {old}"))?;
    let b = commit_of(root, new).ok_or_else(|| format!("git doesn't know {new}"))?;
    let base = Some(said(root, &["merge-base", &a, &b])).filter(|h| h.len() >= 7);
    let swapped = order && a != b && base.as_deref() == Some(b.as_str());
    let (a, b) = if swapped { (b, a) } else { (a, b) };
    let counts = said(root, &["rev-list", "--count", "--left-right", &format!("{a}...{b}"), "--"]);
    let mut it = counts.split_whitespace().filter_map(|n| n.parse().ok());
    let counts = [it.next().unwrap_or(0), it.next().unwrap_or(0)];
    let lists = [only(root, &a, &b), only(root, &b, &a)];
    Ok(Sides { hashes: [a, b], base, only: lists, counts, swapped })
}

/// "3 commits only on topic", over each side's list.
pub fn heading(n: usize, name: &str) -> String {
    match n {
        0 => format!("Nothing only on {name}"),
        1 => format!("1 commit only on {name}"),
        n => format!("{} commits only on {name}", crate::table::fmt_count(n)),
    }
}
