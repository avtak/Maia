//! Background file operations with progress reporting.

use std::fs;
use std::io::{Read, Write};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::fsx::unique_dest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// Keep both: "name (1).ext".
    KeepBoth,
    Overwrite,
    /// Replace only when the one coming is newer; else skip it.
    Newer,
    Skip,
    /// Folders only: copy into the existing folder, replacing files.
    Merge,
    /// Land under this name instead.
    Rename(String),
}

#[derive(Clone, Debug)]
pub enum OpKind {
    Copy { srcs: Vec<PathBuf>, dest: PathBuf, decide: HashMap<PathBuf, Conflict> },
    Move { srcs: Vec<PathBuf>, dest: PathBuf, decide: HashMap<PathBuf, Conflict> },
    Trash { paths: Vec<PathBuf> },
    Delete { paths: Vec<PathBuf> },
    /// Paths inside `archive` (none: all of it, into its own folder when it holds
    /// several things) extracted into `dest`.
    Extract { archive: PathBuf, members: Vec<PathBuf>, dest: PathBuf },
    /// `srcs` packed into the archive `out` at `level`, locked with a password when there
    /// is one.
    Compress { srcs: Vec<PathBuf>, out: PathBuf, password: Option<crate::archive::Secret>, level: crate::archwrite::Level },
    /// A change inside `archive`, made beside it and swapped in once whole (the old one
    /// to the trash).
    ArchiveEdit { archive: PathBuf, edit: crate::archwrite::Edit },
    /// One command run as administrator (through pkexec).
    Admin { task: crate::admin::Task, tools: crate::drives::Tools },
    /// Files converted to another format beside them (all into `out`, when one file is
    /// made of several). The tools travel with the job: a test's fakes are only known
    /// to the thread that set them.
    Convert { srcs: Vec<PathBuf>, to: crate::convert::Target, out: Option<PathBuf>, tools: crate::drives::Tools },
    /// Each file moved into its own folder (made when missing): Tidy Folder.
    Sort { moves: Vec<(PathBuf, PathBuf)>, label: String },
    /// A folder synced into another: each (from, to) copied, over what is at `to` (which
    /// goes to the trash); `remove` to the trash after (mirroring).
    Sync { copies: Vec<(PathBuf, PathBuf)>, remove: Vec<PathBuf> },
    /// Permission bits set and cleared, and a new owner and group, on `paths` (and, when
    /// `deep`, everything inside them).
    Perms { paths: Vec<PathBuf>, change: crate::perms::Change, deep: bool },
    /// Each `from` moved to exactly `to`: Undo and Redo of a move, which must land where
    /// it was. A `to` that is taken is refused, unless it is in `over` (it goes to the
    /// trash first); once all are moved, each of `back` returns from the trash.
    Place { pairs: Vec<(PathBuf, PathBuf)>, over: Vec<PathBuf>, back: Vec<PathBuf>, label: String },
}

impl OpKind {
    pub fn verb(&self) -> &'static str {
        match self {
            OpKind::Copy { .. } => "Copying",
            OpKind::Move { .. } => "Moving",
            OpKind::Trash { .. } => "Moving to trash",
            OpKind::Delete { .. } => "Deleting",
            OpKind::Extract { .. } => "Extracting",
            OpKind::Compress { .. } => "Compressing",
            OpKind::ArchiveEdit { .. } => "Changing",
            OpKind::Admin { .. } => "Working as administrator",
            OpKind::Convert { .. } => "Converting",
            OpKind::Sort { .. } => "Moving",
            OpKind::Sync { .. } => "Syncing",
            OpKind::Perms { .. } => "Changing permissions of",
            OpKind::Place { .. } => "Moving",
        }
    }
}

#[derive(Default, Debug)]
pub struct Progress {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: usize,
    pub done_items: usize,
    pub current: String,
    pub errors: Vec<String>,
    pub finished: bool,
    /// (src, dst) pairs actually produced, for undo.
    pub produced: Vec<(PathBuf, PathBuf)>,
    /// What those replaced, now in the trash (Undo brings them back).
    pub replaced: Vec<PathBuf>,
    /// Items refused for lack of permission (offered again as administrator).
    pub denied: Vec<PathBuf>,
    /// Items something else was using (what holds them is offered).
    pub busy: Vec<PathBuf>,
    /// An archive that wants a password (or refused the one given): asked for, then the
    /// job runs again.
    pub password: Option<(PathBuf, bool)>,
    /// How far into the item at hand a job that can't count bytes is (a conversion), from
    /// 0 to 1.
    pub within: Option<f32>,
    /// Folders the job made, outermost first.
    pub made: Vec<PathBuf>,
    /// Paths no longer where they were: moved away, trashed or deleted.
    pub gone: Vec<PathBuf>,
    /// What came back from the trash (a move undone over what it had replaced).
    pub restored: Vec<PathBuf>,
    /// Waiting for the disk: the label of the job it waits for.
    pub waiting: Option<String>,
    /// Asked to start without waiting its turn.
    pub go: bool,
    /// When it started after waiting its turn (speed and time left count from here).
    pub since: Option<std::time::Instant>,
}

pub struct Job {
    pub id: u64,
    pub label: String,
    pub kind: OpKind,
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: Arc<AtomicBool>,
    pub pause: Arc<AtomicBool>,
    pub started: std::time::Instant,
    pub finished_at: Option<std::time::Instant>,
    pub show_errors: bool,
}

pub fn spawn(id: u64, kind: OpKind, ctx: egui::Context) -> Job {
    spawn_in(id, kind, ctx, &Arc::new(Queue::default()), false)
}

/// A job that takes its turn in `queue` when it moves data, and reads its copies back
/// from the disk when `check` is on.
pub fn spawn_in(id: u64, kind: OpKind, ctx: egui::Context, queue: &Arc<Queue>, check: bool) -> Job {
    let progress = Arc::new(Mutex::new(Progress::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let n = match &kind {
        OpKind::Copy { srcs, .. } | OpKind::Move { srcs, .. } | OpKind::Compress { srcs, .. } | OpKind::Convert { srcs, .. } => srcs.len(),
        OpKind::Trash { paths } | OpKind::Delete { paths } | OpKind::Extract { members: paths, .. } => paths.len(),
        OpKind::Admin { .. } => 1,
        OpKind::ArchiveEdit { edit, .. } => edit.count(),
        OpKind::Sort { moves, .. } => moves.len(),
        OpKind::Sync { copies, remove } => copies.len() + remove.len(),
        OpKind::Perms { paths, .. } => paths.len(),
        OpKind::Place { pairs, .. } => pairs.len(),
    };
    let label = match &kind {
        OpKind::Extract { archive, members, .. } if members.is_empty() => format!("Extracting {}", name_of(archive)),
        OpKind::Compress { out, .. } => format!("Compressing into {}", name_of(out)),
        OpKind::ArchiveEdit { archive, edit } => crate::archwrite::label(archive, edit),
        OpKind::Admin { task, .. } => task.label(),
        OpKind::Convert { srcs, to, .. } => crate::convert::label(srcs, *to),
        OpKind::Sort { label, .. } | OpKind::Place { label, .. } => label.clone(),
        OpKind::Sync { copies, .. } => format!("Syncing into {}", copies.first().and_then(|c| c.1.parent()).map(name_of).unwrap_or_default()),
        // One file by its name, several by their number: "Moving notes.txt to the trash".
        _ => {
            let first = match &kind {
                OpKind::Copy { srcs, .. } | OpKind::Move { srcs, .. } | OpKind::Perms { paths: srcs, .. } => srcs.first(),
                OpKind::Trash { paths } | OpKind::Delete { paths } | OpKind::Extract { members: paths, .. } => paths.first(),
                _ => None,
            };
            let what = match first {
                Some(p) if n == 1 => name_of(p),
                _ => format!("{n} item{}", if n == 1 { "" } else { "s" }),
            };
            match &kind {
                OpKind::Trash { .. } => format!("Moving {what} to the trash"),
                _ => format!("{} {what}", kind.verb()),
            }
        }
    };
    // Counted before it starts, for the card of one waiting its turn.
    progress.lock().unwrap().total_items = n;
    let (p2, c2, k2, pa2, l2) = (progress.clone(), cancel.clone(), kind.clone(), pause.clone(), label.clone());
    let queue = queue.clone();
    std::thread::spawn(move || {
        let check = check.then(|| Check { keys: std::collections::hash_map::RandomState::new(), written: Default::default() });
        let gate = Gate { cancel: &c2, pause: &pa2, check };
        let disks = disks_of(&k2);
        let turn = disks.is_empty() || queue.wait_turn(id, &l2, disks, &p2, &gate, &ctx);
        if turn {
            run(&k2, &p2, &gate, &ctx);
        }
        queue.done(id);
        p2.lock().unwrap().finished = true;
        ctx.request_repaint();
    });
    Job { id, label, kind, progress, cancel, pause, started: std::time::Instant::now(), finished_at: None, show_errors: false }
}

/// Cancellation + pause checkpoint shared by the workers.
pub struct Gate<'a> {
    cancel: &'a AtomicBool,
    pause: &'a AtomicBool,
    /// Copies to read back once they are on the disk (Check Copies).
    check: Option<Check>,
}

/// What each file copied hashed to as its bytes went through, to compare with what the
/// disk gives back. The hash is keyed afresh for every job, so no corruption can be
/// shaped to match it.
pub(crate) struct Check {
    keys: std::collections::hash_map::RandomState,
    written: std::cell::RefCell<Vec<(PathBuf, u64)>>,
}

#[cfg(test)]
impl<'a> Gate<'a> {
    /// A gate for work run on the test's own thread.
    pub fn new(cancel: &'a AtomicBool, pause: &'a AtomicBool) -> Gate<'a> {
        Gate { cancel, pause, check: None }
    }
}

impl Gate<'_> {
    /// Blocks while paused; returns true if cancelled.
    pub(crate) fn stop(&self) -> bool {
        while self.pause.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        self.cancel.load(Ordering::Relaxed)
    }

    /// For work another process does, which is stopped and continued instead of waited on.
    pub(crate) fn paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// How many times each byte is handled: twice when copies are read back.
    fn weight(&self) -> u64 {
        if self.check.is_some() { 2 } else { 1 }
    }

    /// A part renamed into place: its files are checked under their new names.
    fn renamed(&self, from: &Path, to: &Path) {
        if let Some(c) = &self.check {
            for (f, _) in c.written.borrow_mut().iter_mut() {
                if let Ok(rest) = f.strip_prefix(from) {
                    *f = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
                }
            }
        }
    }
}

fn run(kind: &OpKind, p: &Mutex<Progress>, cancel: &Gate, ctx: &egui::Context) {
    match kind {
        OpKind::Copy { srcs, dest, decide } => {
            let total: u64 = srcs.iter().map(|s| tree_size(s)).sum();
            {
                let mut g = p.lock().unwrap();
                g.total_bytes = total * cancel.weight();
                g.total_items = srcs.len();
            }
            for s in srcs {
                if cancel.stop() {
                    break;
                }
                let policy = decide.get(s).cloned().unwrap_or(Conflict::KeepBoth);
                let Some(d) = resolve_dest(s, dest, &policy, p) else { continue };
                if s.is_dir() && inside(d.landed(), s) {
                    p.lock().unwrap().errors.push(format!("Can't copy {} into itself", s.display()));
                    continue;
                }
                let merge = policy == Conflict::Merge && d.replace.is_none();
                let fresh = fs::symlink_metadata(&d.to).is_err();
                let r = checked(cancel, p, &d.to, copy_tree(s, &d.to, p, cancel, ctx, merge, false)).and_then(|_| match &d.replace {
                    Some(t) => swap_in(&d.to, t),
                    None => Ok(()),
                });
                match r {
                    Ok(()) => {
                        let mut g = p.lock().unwrap();
                        g.produced.push((s.clone(), d.landed().to_path_buf()));
                        g.replaced.extend(d.replace.clone());
                    }
                    Err(e) => {
                        // Half a copy is no copy: what this job made goes, nothing else.
                        if fresh {
                            let _ = remove_tree(&d.to);
                        }
                        let mut g = p.lock().unwrap();
                        if crate::admin::denied(&e) {
                            g.denied.push(s.clone());
                        }
                        if crate::holders::is_busy(&e) {
                            g.busy.push(s.clone());
                        }
                        g.errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
            }
        }
        OpKind::Move { srcs, dest, decide } => {
            p.lock().unwrap().total_items = srcs.len();
            let mut later = vec![];
            for s in srcs {
                if cancel.stop() {
                    break;
                }
                move_one(s, dest, decide.get(s).cloned().unwrap_or(Conflict::KeepBoth), p, cancel, ctx, &mut later);
            }
            finish_moves(later, p);
        }
        OpKind::Sort { moves, .. } => {
            p.lock().unwrap().total_items = moves.len();
            let mut later = vec![];
            for (s, dest) in moves {
                if cancel.stop() {
                    break;
                }
                // The folders it makes, outermost first, so Undo can take them away again.
                if !dest.is_dir() {
                    let mut new = vec![];
                    let mut d = dest.as_path();
                    while !d.exists() {
                        new.push(d.to_path_buf());
                        let Some(up) = d.parent() else { break };
                        d = up;
                    }
                    if let Err(e) = fs::create_dir_all(dest) {
                        let mut g = p.lock().unwrap();
                        g.errors.push(format!("{}: {e}", dest.display()));
                        g.done_items += 1;
                        continue;
                    }
                    p.lock().unwrap().made.extend(new.into_iter().rev());
                }
                move_one(s, dest, Conflict::KeepBoth, p, cancel, ctx, &mut later);
            }
            finish_moves(later, p);
        }
        OpKind::Sync { copies, remove } => {
            {
                let mut g = p.lock().unwrap();
                g.total_bytes = copies.iter().map(|(s, _)| tree_size(s)).sum::<u64>() * cancel.weight();
                g.total_items = copies.len() + remove.len();
            }
            for (s, d) in copies {
                if cancel.stop() {
                    return;
                }
                // Over something: written beside it and swapped in once whole.
                let over = fs::symlink_metadata(d).is_ok();
                let to = if over { unique_dest(d.parent().unwrap_or(d), &format!(".{}.maia-part", name_of(d))) } else { d.clone() };
                match checked(cancel, p, &to, copy_tree(s, &to, p, cancel, ctx, false, false)).and_then(|_| if over { swap_in(&to, d) } else { Ok(()) }) {
                    Ok(()) => {
                        let mut g = p.lock().unwrap();
                        g.produced.push((s.clone(), d.clone()));
                        if over {
                            g.replaced.push(d.clone());
                        }
                    }
                    Err(e) => {
                        let _ = remove_tree(&to);
                        p.lock().unwrap().errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
            }
            for r in remove {
                if cancel.stop() {
                    return;
                }
                match crate::trashcan::delete(r) {
                    Ok(()) => p.lock().unwrap().gone.push(r.clone()),
                    Err(e) => p.lock().unwrap().errors.push(format!("{}: {e}", r.display())),
                }
                p.lock().unwrap().done_items += 1;
            }
        }
        OpKind::Place { pairs, over, back, .. } => {
            p.lock().unwrap().total_items = pairs.len();
            let mut later = vec![];
            for (from, to) in pairs {
                if cancel.stop() {
                    break;
                }
                place_one(from, to, over.contains(to), p, cancel, ctx, &mut later);
            }
            finish_moves(later, p);
            // What the move had replaced, back in its place now that it is free.
            for r in back {
                match crate::picfix::newest_trashed(r) {
                    Some(it) if fs::symlink_metadata(r).is_err() => match crate::trashcan::restore(&it) {
                        Ok(()) => p.lock().unwrap().restored.push(r.clone()),
                        Err(e) => p.lock().unwrap().errors.push(format!("{}: {e}", r.display())),
                    },
                    _ => p.lock().unwrap().errors.push(format!("{}: the one it replaced is not in the trash", r.display())),
                }
            }
        }
        OpKind::Perms { paths, change, deep } => {
            p.lock().unwrap().total_items = paths.len();
            for s in paths {
                if cancel.stop() {
                    return;
                }
                p.lock().unwrap().current = name_of(s);
                let errs = crate::perms::apply(s, change, *deep, cancel);
                let mut g = p.lock().unwrap();
                // Refused anywhere in it: offered again, as administrator, as a whole.
                if errs.iter().any(|e| crate::admin::denied(&e.1)) {
                    g.denied.push(s.clone());
                }
                g.errors.extend(errs.iter().take(20).map(|(at, e)| format!("{}: {e}", at.display())));
                g.done_items += 1;
            }
        }
        OpKind::Trash { paths } => {
            p.lock().unwrap().total_items = paths.len();
            for s in paths {
                if cancel.stop() {
                    break;
                }
                p.lock().unwrap().current = name_of(s);
                match crate::trashcan::delete(s) {
                    Ok(()) => p.lock().unwrap().gone.push(s.clone()),
                    Err(e) => {
                        let mut g = p.lock().unwrap();
                        if crate::holders::is_busy(&e) {
                            g.busy.push(s.clone());
                        }
                        g.errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
                ctx.request_repaint();
            }
        }
        OpKind::Admin { task, tools } => {
            p.lock().unwrap().total_items = 1;
            match crate::admin::run(tools, task) {
                Ok(()) => {
                    use crate::admin::Task;
                    let mut g = p.lock().unwrap();
                    g.done_items = 1;
                    match task {
                        Task::Move { srcs: v, .. } | Task::Delete { paths: v } => g.gone.extend(v.iter().cloned()),
                        Task::Rename { from, .. } => g.gone.push(from.clone()),
                        _ => {}
                    }
                }
                Err(e) => p.lock().unwrap().errors.push(e),
            }
        }
        OpKind::Convert { srcs, to, out, tools } => crate::convert::run(srcs, *to, out.as_deref(), tools, p, cancel, ctx),
        OpKind::Extract { archive, members, dest } => {
            p.lock().unwrap().current = name_of(archive);
            extract(archive, members, dest, p, cancel, ctx);
        }
        OpKind::Compress { srcs, out, password, level } => {
            p.lock().unwrap().total_items = srcs.len();
            p.lock().unwrap().current = name_of(out);
            match crate::archwrite::compress(out, srcs, *level, password.as_ref().map(|pw| pw.0.as_str()), p, cancel, ctx) {
                Ok(()) => {
                    let mut g = p.lock().unwrap();
                    g.produced.push((out.clone(), out.clone()));
                    g.done_items = srcs.len();
                }
                Err(_) if cancel.cancelled() => {}
                Err(e) => p.lock().unwrap().errors.push(format!("{}: {e}", name_of(out))),
            }
        }
        OpKind::ArchiveEdit { archive, edit } => {
            p.lock().unwrap().current = name_of(archive);
            match crate::archwrite::edit(archive, edit, p, cancel, ctx) {
                Ok(()) => {
                    let mut g = p.lock().unwrap();
                    g.replaced.push(archive.clone());
                    g.done_items = g.total_items;
                }
                Err(_) if cancel.cancelled() => {}
                Err(e) => {
                    let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
                    if crate::archive::wants_password(&e) {
                        g.password = Some((archive.clone(), e == crate::archive::WRONG_PASSWORD));
                    }
                    g.errors.push(format!("{}: {e}", name_of(archive)));
                }
            }
        }
        OpKind::Delete { paths } => {
            p.lock().unwrap().total_items = paths.len();
            for s in paths {
                if cancel.stop() {
                    break;
                }
                p.lock().unwrap().current = name_of(s);
                match remove_tree(s) {
                    Ok(()) => p.lock().unwrap().gone.push(s.clone()),
                    Err(e) => {
                        let mut g = p.lock().unwrap();
                        if crate::admin::denied(&e) {
                            g.denied.push(s.clone());
                        }
                        if crate::holders::is_busy(&e) {
                            g.busy.push(s.clone());
                        }
                        g.errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
                ctx.request_repaint();
            }
        }
    }
}

/// What a finished job changed, for every listing that shows it.
#[derive(Default, Debug, PartialEq)]
pub struct Changed {
    /// Folders things arrived in.
    pub arrived: Vec<PathBuf>,
    /// Folders things left.
    pub left: Vec<PathBuf>,
    /// Paths no longer where they were.
    pub gone: Vec<PathBuf>,
}

impl Changed {
    fn add(v: &mut Vec<PathBuf>, d: Option<&Path>) {
        if let Some(d) = d.filter(|d| !v.iter().any(|x| x == d)) {
            v.push(d.to_path_buf());
        }
    }

    /// Every folder whose contents changed.
    pub fn dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.arrived.iter().chain(self.left.iter().filter(|d| !self.arrived.contains(d)))
    }

    pub fn is_empty(&self) -> bool {
        self.arrived.is_empty() && self.left.is_empty()
    }

    /// Things moved from `pairs`' first paths to their second.
    pub fn moved(pairs: &[(PathBuf, PathBuf)]) -> Changed {
        let mut c = Changed::default();
        for (from, to) in pairs {
            Changed::add(&mut c.left, from.parent());
            Changed::add(&mut c.arrived, to.parent());
            c.gone.push(from.clone());
        }
        c
    }

    /// `arrived` came into their folders, `gone` left theirs.
    pub fn at(arrived: &[PathBuf], gone: &[PathBuf]) -> Changed {
        let mut c = Changed { gone: gone.to_vec(), ..Default::default() };
        arrived.iter().for_each(|p| Changed::add(&mut c.arrived, p.parent()));
        gone.iter().for_each(|p| Changed::add(&mut c.left, p.parent()));
        c
    }

    /// What a job of `kind` changed, by what it recorded as it went.
    pub fn of(kind: &OpKind, p: &Progress) -> Changed {
        use crate::admin::Task;
        let mut c = Changed { gone: p.gone.clone(), ..Default::default() };
        let parents = |v: &mut Vec<PathBuf>, paths: &[PathBuf]| paths.iter().for_each(|s| Changed::add(v, s.parent()));
        match kind {
            OpKind::Copy { dest, .. } | OpKind::Extract { dest, .. } | OpKind::Admin { task: Task::Copy { dest, .. }, .. } => c.arrived.push(dest.clone()),
            OpKind::Move { srcs, dest, .. } | OpKind::Admin { task: Task::Move { srcs, dest }, .. } => {
                c.arrived.push(dest.clone());
                parents(&mut c.left, srcs);
            }
            OpKind::Sort { moves, .. } => {
                for (s, d) in moves {
                    Changed::add(&mut c.left, s.parent());
                    Changed::add(&mut c.arrived, Some(d));
                }
            }
            OpKind::Place { pairs, .. } => {
                for (s, d) in pairs {
                    Changed::add(&mut c.left, s.parent());
                    Changed::add(&mut c.arrived, d.parent());
                }
            }
            OpKind::Trash { paths } | OpKind::Delete { paths } | OpKind::Admin { task: Task::Delete { paths }, .. } => parents(&mut c.left, paths),
            OpKind::Compress { out, .. } => Changed::add(&mut c.arrived, out.parent()),
            // The archive is a folder to the tabs inside it, and changed in the one it is in.
            OpKind::ArchiveEdit { archive, .. } => {
                Changed::add(&mut c.arrived, Some(archive));
                Changed::add(&mut c.arrived, archive.parent());
            }
            OpKind::Sync { copies, remove } => {
                copies.iter().for_each(|(_, d)| Changed::add(&mut c.arrived, d.parent()));
                parents(&mut c.left, remove);
            }
            OpKind::Convert { srcs, .. } => Changed::add(&mut c.arrived, srcs.first().and_then(|s| s.parent())),
            OpKind::Admin { task: Task::NewFolder(p) | Task::NewFile(p), .. } => Changed::add(&mut c.arrived, p.parent()),
            OpKind::Admin { task: Task::Rename { from, .. }, .. } => Changed::add(&mut c.arrived, from.parent()),
            OpKind::Perms { paths, .. } | OpKind::Admin { task: Task::Perms { paths, .. }, .. } => parents(&mut c.arrived, paths),
        }
        c
    }
}

/// Move `s` into `dest`, recording what it became (or why it couldn't). A move to another
/// filesystem is a copy whose original is deleted by `finish_moves`, once queued in `later`.
fn move_one(s: &PathBuf, dest: &Path, policy: Conflict, p: &Mutex<Progress>, cancel: &Gate, ctx: &egui::Context, later: &mut Vec<Later>) {
    if s.parent() == Some(dest) {
        p.lock().unwrap().done_items += 1;
        return; // moving onto itself is a no-op
    }
    if inside(dest, s) {
        p.lock().unwrap().errors.push(format!("Can't move {} into itself", s.display()));
        return;
    }
    let Some(d) = resolve_dest(s, dest, &policy, p) else { return };
    p.lock().unwrap().current = name_of(s);
    let merge = policy == Conflict::Merge && d.replace.is_none() && d.to.is_dir();
    let fresh = fs::symlink_metadata(&d.to).is_err();
    let r = match if merge { Err(std::io::Error::from_raw_os_error(libc::EXDEV)) } else { crate::fsx::rename_new(s, &d.to) } {
        Ok(()) => match &d.replace {
            Some(t) => swap_in(&d.to, t).inspect_err(|_| {
                // Not swapped in: back where it came from rather than left half-placed.
                let _ = fs::rename(&d.to, s);
            }),
            None => Ok(()),
        },
        // Cross-device: copy and put in place; the original goes later.
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            p.lock().unwrap().total_bytes += tree_size(s) * cancel.weight();
            let skipped = p.lock().unwrap().errors.len();
            let r = checked(cancel, p, &d.to, copy_tree(s, &d.to, p, cancel, ctx, merge, true)).and_then(|_| match &d.replace {
                Some(t) => swap_in(&d.to, t),
                None => Ok(()),
            });
            if r.is_err() && fresh {
                let _ = remove_tree(&d.to);
            }
            // Something inside was skipped (a special file, an unreadable entry): the copy
            // is not the whole of it, so the original stays beside it.
            let whole = || {
                if p.lock().unwrap().errors.len() > skipped {
                    return Err(std::io::Error::other("not all of it could be copied, so the original was kept"));
                }
                Ok(())
            };
            match r.and_then(|_| whole()) {
                Ok(()) => {
                    later.push(Later { src: s.clone(), dest: dest.to_path_buf(), landed: d.landed().to_path_buf(), replaced: d.replace.clone() });
                    p.lock().unwrap().done_items += 1;
                    ctx.request_repaint();
                    return;
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    record_move(p, s, r, d.landed(), d.replace.clone());
    p.lock().unwrap().done_items += 1;
    ctx.request_repaint();
}

/// A cross-device move whose copy is whole and in place, waiting for its original to go.
struct Later {
    src: PathBuf,
    dest: PathBuf,
    landed: PathBuf,
    replaced: Option<PathBuf>,
}

/// Delete the originals of cross-device moves, once their copies are on the destination's
/// disk and not only in the page cache, where a write error (a stick pulled out) would
/// surface after the original was gone. One syncfs per destination, not one per item:
/// each waits on the device, and moving a thousand photos to a stick is a thousand items.
fn finish_moves(later: Vec<Later>, p: &Mutex<Progress>) {
    let mut flushed: Vec<(PathBuf, Result<(), String>)> = vec![];
    for l in later {
        let f = match flushed.iter().find(|(d, _)| *d == l.dest) {
            Some((_, f)) => f.clone(),
            None => {
                let f = syncfs(&l.dest).map_err(|e| format!("the copy may not be on the disk yet, so the original was kept: {e}"));
                flushed.push((l.dest.clone(), f.clone()));
                f
            }
        };
        let r = f.map_err(std::io::Error::other).and_then(|_| remove_tree(&l.src));
        record_move(p, &l.src, r, &l.landed, l.replaced);
    }
}

fn syncfs(dir: &Path) -> std::io::Result<()> {
    let f = fs::File::open(dir)?;
    match unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&f)) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// What a finished move did, for the listings and Undo, or why it failed.
fn record_move(p: &Mutex<Progress>, s: &Path, r: std::io::Result<()>, landed: &Path, replaced: Option<PathBuf>) {
    let mut g = p.lock().unwrap();
    match r {
        Ok(()) => {
            g.gone.push(s.to_path_buf());
            g.produced.push((s.to_path_buf(), landed.to_path_buf()));
            g.replaced.extend(replaced);
        }
        Err(e) => {
            if crate::admin::denied(&e) {
                g.denied.push(s.to_path_buf());
            }
            if crate::holders::is_busy(&e) {
                g.busy.push(s.to_path_buf());
            }
            g.errors.push(format!("{}: {e}", s.display()));
        }
    }
}

/// Extract into a hidden staging folder beside the results, then move each top-level
/// result into place (renamed if the name is taken), so a failure leaves nothing half done.
fn extract(archive: &Path, members: &[PathBuf], dest: &Path, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context) {
    let fail = |e: String| {
        let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
        if crate::archive::wants_password(&e) {
            g.password = Some((archive.to_path_buf(), e == crate::archive::WRONG_PASSWORD));
        }
        g.errors.push(format!("{}: {e}", name_of(archive)));
    };
    // The folder made for several top-level items, removed again if nothing lands in it.
    let mut made: Option<PathBuf> = None;
    let (members, dest) = if members.is_empty() {
        let tops: Vec<PathBuf> = match crate::archive::list(archive) {
            Ok(all) => all.iter().filter(|i| i.path.components().count() == 1).map(|i| i.path.clone()).collect(),
            Err(e) => return fail(e),
        };
        // Several things at the top go into a folder named after the archive.
        let dest = if tops.len() > 1 {
            let stem = name_of(archive);
            let stem = crate::archive::stem(&stem);
            let d = unique_dest(dest, stem);
            if let Err(e) = fs::create_dir(&d) {
                return fail(e.to_string());
            }
            made = Some(d.clone());
            d
        } else {
            dest.to_path_buf()
        };
        (tops, dest)
    } else {
        (members.to_vec(), dest.to_path_buf())
    };
    p.lock().unwrap_or_else(|e| e.into_inner()).total_items = members.len();
    let stage = dest.join(format!(".maia-extract-{}", std::process::id()));
    if let Err(e) = crate::archwrite::extract(archive, &members, &stage, p, gate, ctx) {
        let _ = fs::remove_dir_all(&stage);
        if let Some(d) = made {
            let _ = fs::remove_dir(d);
        }
        if gate.cancelled() {
            return;
        }
        return fail(e);
    }
    for m in &members {
        let from = stage.join(m);
        let to = unique_dest(&dest, &name_of(m));
        match fs::rename(&from, &to) {
            Ok(()) => p.lock().unwrap_or_else(|e| e.into_inner()).produced.push((archive.join(m), to)),
            Err(e) => fail(format!("{}: {e}", m.display())),
        }
        p.lock().unwrap_or_else(|e| e.into_inner()).done_items += 1;
    }
    let _ = fs::remove_dir_all(&stage);
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Where an item goes: `to`, and when it replaces something there, what. A replacement
/// is written beside it under a hidden name and swapped in only once whole, so a copy
/// that fails or is cancelled halfway leaves what was there as it was.
struct Dest {
    to: PathBuf,
    replace: Option<PathBuf>,
}

impl Dest {
    /// The name the item has once it is in place.
    fn landed(&self) -> &Path {
        self.replace.as_deref().unwrap_or(&self.to)
    }
}

fn resolve_dest(src: &Path, dest_dir: &Path, conflict: &Conflict, p: &Mutex<Progress>) -> Option<Dest> {
    let name = match conflict {
        Conflict::Rename(n) => n.clone(),
        _ => name_of(src),
    };
    let target = dest_dir.join(&name);
    let exists = fs::symlink_metadata(&target).is_ok();
    // Copying a file onto itself always duplicates.
    if !exists || target == src {
        return Some(Dest { to: if exists { unique_dest(dest_dir, &name) } else { target }, replace: None });
    }
    let newer = || fs::metadata(src).and_then(|m| m.modified()).ok() > fs::metadata(&target).and_then(|m| m.modified()).ok().and_then(|t| t.checked_add(std::time::Duration::from_secs(1)));
    match conflict {
        Conflict::KeepBoth | Conflict::Rename(_) => Some(Dest { to: unique_dest(dest_dir, &name), replace: None }),
        Conflict::Newer if !newer() => {
            p.lock().unwrap().done_items += 1;
            None
        }
        Conflict::Skip => {
            p.lock().unwrap().done_items += 1;
            None
        }
        Conflict::Merge if target.is_dir() && src.is_dir() => Some(Dest { to: target, replace: None }),
        Conflict::Merge | Conflict::Overwrite | Conflict::Newer => Some(Dest { to: unique_dest(dest_dir, &format!(".{name}.maia-part")), replace: Some(target) }),
    }
}

/// Put a finished part in place of what it replaces: the old one to the trash, where it
/// can be had back (removed only when the trash can't take it), then the part renamed.
fn swap_in(part: &Path, target: &Path) -> std::io::Result<()> {
    if crate::trashcan::delete(target).is_err() {
        remove_tree(target)?;
    }
    fs::rename(part, target)
}

/// Is `p` the same as or inside `dir`, also through symlinks (compared by real path)?
fn inside(p: &Path, dir: &Path) -> bool {
    if p.starts_with(dir) {
        return true;
    }
    let real = |x: &Path| -> Option<PathBuf> {
        // `p` may not exist yet: resolve its closest existing ancestor.
        let mut base = x.to_path_buf();
        let mut rest = vec![];
        while !base.exists() {
            rest.push(base.file_name()?.to_os_string());
            base = base.parent()?.to_path_buf();
        }
        let mut r = fs::canonicalize(&base).ok()?;
        for c in rest.iter().rev() {
            r.push(c);
        }
        Some(r)
    };
    match (real(p), real(dir)) {
        (Some(a), Some(b)) => a.starts_with(&b),
        _ => false,
    }
}

/// Total size of files below `p` (symlinks not followed). Iterative, so arbitrarily deep
/// trees can't overflow the stack.
pub fn tree_size(p: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&d) else { continue };
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(&d) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else {
            total += m.len();
        }
    }
    total
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    p: &Mutex<Progress>,
    cancel: &Gate,
    ctx: &egui::Context,
    merge: bool,
    // A move keeps the SELinux label it had; a copy takes the one of where it lands, as
    // with `cp` and `mv`.
    label: bool,
) -> std::io::Result<()> {
    if cancel.stop() {
        return Err(std::io::Error::other("cancelled"));
    }
    let m = fs::symlink_metadata(src)?;
    let ft = m.file_type();
    {
        use std::os::unix::fs::FileTypeExt;
        // Devices, FIFOs and sockets: reading them can block forever or never end
        // (/dev/zero would fill the disk). Skip them with an error instead.
        if ft.is_fifo() || ft.is_socket() || ft.is_char_device() || ft.is_block_device() {
            p.lock().unwrap().errors.push(format!("Skipped special file {}", src.display()));
            return Ok(());
        }
    }
    if ft.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        if !(merge && dst.is_dir()) {
            fs::create_dir(dst)?;
        }
        for de in fs::read_dir(src)? {
            // An entry that can't be read is reported rather than silently left out, and
            // the rest is still copied; a move then keeps its original.
            let de = match de {
                Ok(de) => de,
                Err(e) => {
                    p.lock().unwrap().errors.push(format!("{}: {e}", src.display()));
                    continue;
                }
            };
            let child = dst.join(de.file_name());
            let there = fs::symlink_metadata(&child).ok();
            if merge && there.as_ref().is_some_and(|m| !m.is_dir()) {
                if de.file_type().is_ok_and(|t| t.is_dir()) {
                    // A folder where a file was: the file goes first, as before.
                    fs::remove_file(&child)?;
                } else {
                    // A file over a file: written beside it, renamed over it once whole
                    // (a rename replaces in one step), so a failure keeps the old one.
                    let part = unique_dest(dst, &format!(".{}.maia-part", de.file_name().to_string_lossy()));
                    if let Err(e) = copy_tree(&de.path(), &part, p, cancel, ctx, false, label).and_then(|_| fs::rename(&part, &child)) {
                        let _ = remove_tree(&part);
                        return Err(e);
                    }
                    cancel.renamed(&part, &child);
                    continue;
                }
            }
            copy_tree(&de.path(), &child, p, cancel, ctx, merge, label)?;
        }
        xattrs(src, dst, label);
        fs::set_permissions(dst, m.permissions())?;
        if !merge
            && let (Ok(mt), Ok(d)) = (m.modified(), fs::File::open(dst))
        {
            let _ = d.set_modified(mt);
        }
    } else {
        p.lock().unwrap().current = name_of(src);
        let r = fs::File::open(src)?;
        let w = fs::File::create(dst)?;
        match copy_data(&r, &w, m.len(), p, cancel, ctx) {
            Ok(h) => {
                #[cfg(test)]
                spoil(&w, src);
                if let (Some(c), Some(h)) = (&cancel.check, h) {
                    c.written.borrow_mut().push((dst.to_path_buf(), h));
                }
            }
            Err(e) => {
                drop(w);
                // A failed copy (disk full, device gone, cancelled) must not leave a
                // truncated file behind that looks like a finished one.
                let _ = fs::remove_file(dst);
                return Err(e);
            }
        }
        // Before the permissions: a read-only file takes no attributes.
        xattrs(src, dst, label);
        fs::set_permissions(dst, m.permissions())?;
        if let Ok(mt) = m.modified() {
            let _ = w.set_modified(mt);
        }
    }
    Ok(())
}

/// The bytes of `r` into `w`: shared extents where the drive can (btrfs, XFS: instant),
/// else copied in the kernel, else through a buffer. When copies are checked, what they
/// hashed to on the way (the kernel's copy never shows them, so the buffer it is); none
/// for shared extents, where no byte was written that could come back different.
fn copy_data(r: &fs::File, w: &fs::File, len: u64, p: &Mutex<Progress>, cancel: &Gate, ctx: &egui::Context) -> std::io::Result<Option<u64>> {
    use std::hash::{BuildHasher, Hasher};
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let (rf, wf) = (r.as_raw_fd(), w.as_raw_fd());
    if len > 0 && unsafe { libc::ioctl(wf, FICLONE, rf) } == 0 {
        p.lock().unwrap().done_bytes += len * cancel.weight();
        return Ok(None);
    }
    let mut hash = cancel.check.as_ref().map(|c| c.keys.build_hasher());
    let mut since_repaint = 0u64;
    let mut step = |n: u64| {
        p.lock().unwrap().done_bytes += n;
        since_repaint += n;
        if since_repaint > 8 << 20 {
            ctx.request_repaint();
            since_repaint = 0;
        }
    };
    let (mut kernel, mut done) = (hash.is_none(), 0u64);
    let mut buf = if kernel { vec![] } else { vec![0u8; len.clamp(64 << 10, 1 << 20) as usize] };
    loop {
        if cancel.stop() {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = if kernel {
            let n = unsafe { libc::copy_file_range(rf, std::ptr::null_mut(), wf, std::ptr::null_mut(), 4 << 20, 0) };
            let e = std::io::Error::last_os_error();
            // Not offered between these two files, or a file that only tells its size
            // when read (/proc): the plain way, from where it got to.
            if (n < 0 && matches!(e.raw_os_error(), Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM))) || (n == 0 && done == 0) {
                kernel = false;
                buf = vec![0u8; len.clamp(64 << 10, 1 << 20) as usize];
                continue;
            }
            if n < 0 {
                return Err(e);
            }
            n as usize
        } else {
            let n = (&*r).read(&mut buf)?;
            (&*w).write_all(&buf[..n])?;
            if let Some(h) = &mut hash {
                h.write(&buf[..n]);
            }
            n
        };
        if n == 0 {
            return Ok(hash.map(|h| h.finish()));
        }
        done += n as u64;
        step(n as u64);
    }
}

/// Extended attributes (Maia's tags among them) copied over; those the target can't keep
/// (another file system, or `security.*` without the right) are left behind. For a file
/// written in place of `src`, so its SELinux label goes with it.
pub fn copy_xattrs(src: &Path, dst: &Path) {
    xattrs(src, dst, true);
}

/// `copy_xattrs`, with `security.*` (the SELinux label) only when `label`: a copy left
/// with its source's label (from ~/.ssh, say) can be refused to the programs that should
/// read it where it now is.
fn xattrs(src: &Path, dst: &Path, label: bool) {
    use std::os::unix::ffi::OsStrExt;
    let (Ok(s), Ok(d)) = (std::ffi::CString::new(src.as_os_str().as_bytes()), std::ffi::CString::new(dst.as_os_str().as_bytes())) else { return };
    let mut names = vec![0u8; 1024];
    let n = loop {
        let n = unsafe { libc::llistxattr(s.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) && names.len() < 1 << 16 {
            names.resize(names.len() * 4, 0);
            continue;
        }
        break n;
    };
    if n <= 0 {
        return;
    }
    let mut val = vec![0u8; 4096];
    for name in names[..n as usize].split(|&b| b == 0).filter(|n| !n.is_empty() && (label || !n.starts_with(b"security."))) {
        let Ok(name) = std::ffi::CString::new(name) else { continue };
        let mut len = unsafe { libc::lgetxattr(s.as_ptr(), name.as_ptr(), val.as_mut_ptr().cast(), val.len()) };
        if len < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) {
            let need = unsafe { libc::lgetxattr(s.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
            if need > 0 {
                val.resize(need as usize, 0);
                len = unsafe { libc::lgetxattr(s.as_ptr(), name.as_ptr(), val.as_mut_ptr().cast(), val.len()) };
            }
        }
        if len >= 0 {
            unsafe { libc::lsetxattr(d.as_ptr(), name.as_ptr(), val.as_ptr().cast(), len as usize, 0) };
        }
    }
}

/// One file copied for a job, its bytes counted; a copy cut short is removed.
pub(crate) fn copy_file(src: &Path, dst: &Path, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context) -> std::io::Result<()> {
    let r = fs::File::open(src)?;
    let w = fs::File::create_new(dst)?;
    let len = r.metadata()?.len();
    copy_data(&r, &w, len, p, gate, ctx).map(|_| ()).inspect_err(|_| {
        let _ = fs::remove_file(dst);
    })
}

pub fn remove_tree(p: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(p)?;
    if m.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

/// One move of Undo or Redo, to exactly `to`. Across file systems it is a copy whose
/// original goes in `finish_moves`, as for any move; a `to` it was allowed to replace is
/// back from the trash when the move fails.
fn place_one(from: &Path, to: &Path, over: bool, p: &Mutex<Progress>, cancel: &Gate, ctx: &egui::Context, later: &mut Vec<Later>) {
    p.lock().unwrap().current = name_of(from);
    let fail = |e: String| {
        let mut g = p.lock().unwrap();
        g.errors.push(format!("{}: {e}", from.display()));
        g.done_items += 1;
    };
    if fs::symlink_metadata(from).is_err() {
        return fail("no longer there".into());
    }
    let mut trashed = false;
    if fs::symlink_metadata(to).is_ok() {
        if !over {
            return fail(format!("{} is there already", to.display()));
        }
        if let Err(e) = crate::trashcan::delete(to) {
            return fail(e.to_string());
        }
        trashed = true;
    }
    let r = match crate::fsx::rename_new(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            p.lock().unwrap().total_bytes += tree_size(from) * cancel.weight();
            let skipped = p.lock().unwrap().errors.len();
            let r = checked(cancel, p, to, copy_tree(from, to, p, cancel, ctx, false, true)).and_then(|_| {
                if p.lock().unwrap().errors.len() > skipped {
                    return Err(std::io::Error::other("not all of it could be copied, so the original was kept"));
                }
                Ok(())
            });
            match r {
                Ok(()) => {
                    later.push(Later { src: from.to_path_buf(), dest: to.parent().unwrap_or(to).to_path_buf(), landed: to.to_path_buf(), replaced: trashed.then(|| to.to_path_buf()) });
                    p.lock().unwrap().done_items += 1;
                    ctx.request_repaint();
                    return;
                }
                Err(e) => {
                    let _ = remove_tree(to);
                    Err(e)
                }
            }
        }
        r => r,
    };
    if r.is_err() && trashed {
        if let Some(it) = crate::picfix::newest_trashed(to) {
            let _ = crate::trashcan::restore(&it);
        }
    }
    record_move(p, from, r, to, trashed.then(|| to.to_path_buf()));
    p.lock().unwrap().done_items += 1;
    ctx.request_repaint();
}

/// A copy just made, read back from the disk when copies are checked. The files are put
/// on the device first, then their cached pages dropped, so what is read is what the
/// device holds and not what is still in memory. A file that comes back different is
/// removed and the copy fails: for a move, its original stays.
fn checked(gate: &Gate, p: &Mutex<Progress>, at: &Path, r: std::io::Result<()>) -> std::io::Result<()> {
    use std::hash::{BuildHasher, Hasher};
    use std::os::fd::AsRawFd;
    let Some(c) = &gate.check else { return r };
    let files = std::mem::take(&mut *c.written.borrow_mut());
    r?;
    let Some(first) = files.first() else { return Ok(()) };
    // One flush for the whole item: a file's own fsync waits on the device once per file,
    // and a folder of photos is thousands of them.
    syncfs(&first.0).map_err(|e| std::io::Error::other(format!("{}: {e}", at.display())))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut bad = vec![];
    for (f, want) in &files {
        if gate.stop() {
            return Err(std::io::Error::other("cancelled"));
        }
        p.lock().unwrap().current = format!("Checking {}", name_of(f));
        let file = fs::File::open(f)?;
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        let mut h = c.keys.build_hasher();
        loop {
            let n = (&file).read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.write(&buf[..n]);
            p.lock().unwrap().done_bytes += n as u64;
        }
        if h.finish() != *want {
            bad.push(f);
            let _ = fs::remove_file(f);
        }
    }
    match bad.as_slice() {
        [] => Ok(()),
        // The item itself: its name already heads the error.
        [f] if *f == at => Err(std::io::Error::other("the copy read back from the disk differently, so it was removed")),
        [f] => Err(std::io::Error::other(format!("the copy of {} read back from the disk differently, so it was removed", name_of(f)))),
        v => Err(std::io::Error::other(format!("{} copied files read back from the disk differently, so they were removed", v.len()))),
    }
}

/// Test builds: a file named so has its first byte changed once copied, as a failing
/// drive would, for the check to find.
#[cfg(test)]
pub const SPOILED: &str = "spoiled on the way";

#[cfg(test)]
fn spoil(w: &fs::File, src: &Path) {
    use std::os::unix::fs::FileExt;
    if name_of(src).starts_with(SPOILED) {
        let mut b = [0u8; 1];
        let _ = fs::File::open(src).and_then(|f| f.read_at(&mut b, 0));
        let _ = w.write_at(&[b[0] ^ 0xff], 0);
    }
}

/// Jobs that move data take turns on each disk: two copies onto one drive at once take
/// longer than one after the other, as the drive seeks between them, and each shows a
/// time left that is wrong. Jobs on other disks, and quick ones (renames, the trash,
/// permissions), never wait.
#[derive(Default)]
pub struct Queue {
    lanes: Mutex<Vec<Lane>>,
    turn: std::sync::Condvar,
}

struct Lane {
    id: u64,
    label: String,
    disks: Vec<String>,
    running: bool,
}

impl Queue {
    /// Waiting jobs look again: one was told to start now, or cancelled.
    pub fn poke(&self) {
        self.turn.notify_all();
    }

    /// Jobs that hold their disks now.
    pub fn running(&self) -> Vec<u64> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|l| l.running).map(|l| l.id).collect()
    }

    /// Wait until no job started before this one, and none running, uses one of `disks`,
    /// or until the job is told to start anyway. False when it was cancelled first.
    fn wait_turn(&self, id: u64, label: &str, disks: Vec<String>, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context) -> bool {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.push(Lane { id, label: label.to_string(), disks, running: false });
        let mut waited = false;
        loop {
            if gate.cancelled() {
                return false;
            }
            let Some(me) = lanes.iter().position(|l| l.id == id) else { return false };
            let ahead = |l: &&Lane| l.id != id && (l.running || l.id < id) && l.disks.iter().any(|d| lanes[me].disks.contains(d));
            let blocker = lanes.iter().filter(ahead).find(|l| l.running).or_else(|| lanes.iter().find(ahead)).map(|l| l.label.clone());
            let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
            if blocker.is_none() || g.go {
                lanes[me].running = true;
                if g.waiting.take().is_some() || waited {
                    g.since = Some(std::time::Instant::now());
                }
                drop(g);
                ctx.request_repaint();
                return true;
            }
            if g.waiting != blocker {
                g.waiting = blocker;
                ctx.request_repaint();
            }
            drop(g);
            waited = true;
            // Woken when a job ends or is told to start; the timeout covers a cancel.
            lanes = self.turn.wait_timeout(lanes, std::time::Duration::from_millis(250)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn done(&self, id: u64) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.retain(|l| l.id != id);
        drop(lanes);
        self.turn.notify_all();
    }
}

/// The disks a job moves data on, or none for a job that doesn't wait its turn. A move
/// within one file system is a rename, and only its moves to another count.
fn disks_of(kind: &OpKind) -> Vec<String> {
    let paths: Vec<&Path> = match kind {
        OpKind::Copy { srcs, dest, .. } => srcs.iter().map(PathBuf::as_path).chain([dest.as_path()]).collect(),
        OpKind::Move { srcs, dest, decide } => {
            let to = dev_of(dest);
            let far: Vec<&Path> = srcs.iter().filter(|s| dev_of(s) != to || decide.get(*s) == Some(&Conflict::Merge)).map(PathBuf::as_path).collect();
            if far.is_empty() {
                return vec![];
            }
            far.into_iter().chain([dest.as_path()]).collect()
        }
        OpKind::Place { pairs, .. } => pairs.iter().filter(|(f, t)| dev_of(f) != dev_of(t)).flat_map(|(f, t)| [f.as_path(), t.as_path()]).collect(),
        OpKind::Sync { copies, .. } => copies.iter().flat_map(|(f, t)| [f.as_path(), t.as_path()]).collect(),
        OpKind::Extract { archive, dest, .. } => vec![archive.as_path(), dest.as_path()],
        OpKind::Compress { srcs, out, .. } => srcs.iter().map(PathBuf::as_path).chain([out.as_path()]).collect(),
        OpKind::ArchiveEdit { archive, edit } => match edit {
            crate::archwrite::Edit::Add { srcs, .. } => srcs.iter().map(PathBuf::as_path).chain([archive.as_path()]).collect(),
            _ => vec![archive.as_path()],
        },
        _ => vec![],
    };
    let mut devs: Vec<u64> = paths.into_iter().filter_map(dev_of).collect();
    devs.sort_unstable();
    devs.dedup();
    if devs.is_empty() {
        return vec![];
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut disks: Vec<String> = devs.into_iter().map(|d| disk_key((libc::major(d), libc::minor(d)), Path::new("/sys"), &mountinfo)).collect();
    disks.sort();
    disks.dedup();
    disks
}

/// The device of `p`, or of the nearest folder above it that exists.
pub(crate) fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    p.ancestors().find_map(|a| fs::symlink_metadata(a).ok()).map(|m| m.dev())
}

/// The disk `p` is on, by the name the kernel gives it (nvme0n1, sda).
pub fn disk_of(p: &Path) -> Option<String> {
    let d = dev_of(p)?;
    Some(disk_key((libc::major(d), libc::minor(d)), Path::new("/sys"), &fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()))
}

/// The physical disk under device `dev` (major, minor), found in the sysfs at `sys`:
/// a partition, or a mapped device on one (LUKS, LVM), is its disk, so two partitions of
/// one drive share it. A file system with no device of its own (a btrfs subvolume) is
/// found by its mount's source; one on no disk (tmpfs, FUSE, NFS) is a key of its own.
pub fn disk_key(dev: (u32, u32), sys: &Path, mountinfo: &str) -> String {
    let (maj, min) = dev;
    let own = format!("{maj}:{min}");
    let start = if maj != 0 {
        Some(sys.join("dev/block").join(&own))
    } else {
        mountinfo.lines().find_map(|l| {
            if l.split(' ').nth(2)? != own {
                return None;
            }
            let src = l.split(" - ").nth(1)?.split(' ').nth(1)?;
            let dev = Path::new(src.strip_prefix("/dev/").map(|_| src)?);
            // /dev/mapper/root is a link to /dev/dm-0, the name sysfs knows.
            let name = fs::canonicalize(dev).ok().and_then(|r| r.file_name().map(|n| n.to_os_string())).or_else(|| dev.file_name().map(|n| n.to_os_string()))?;
            Some(sys.join("class/block").join(name))
        })
    };
    start.and_then(|s| disk_name(sys, &s)).unwrap_or(own)
}

fn disk_name(sys: &Path, dev: &Path) -> Option<String> {
    let mut d = fs::canonicalize(dev).ok()?;
    // A few levels at most: LVM on LUKS on a partition.
    for _ in 0..6 {
        let under = fs::read_dir(d.join("slaves")).ok().and_then(|r| r.flatten().map(|e| e.file_name()).min());
        match under {
            Some(s) => d = fs::canonicalize(sys.join("class/block").join(s)).ok()?,
            None => break,
        }
    }
    if d.join("partition").exists() {
        d = d.parent()?.to_path_buf();
    }
    Some(d.file_name()?.to_string_lossy().into_owned())
}
