//! Tags and colour labels, kept on the files themselves in the `user.xdg.tags` extended
//! attribute (comma-separated, as KDE's file managers write it), so they travel with the
//! files and other apps see them. A colour label is a tag named after one of seven
//! colours, drawn as a dot.
//!
//! Reading an attribute costs several times a stat, so tags are never read while
//! listing: rows ask for theirs when drawn, and a `tag:` filter asks for every row; a
//! worker reads them and the cache keeps only files that have tags.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, mpsc};

use egui::Color32;

const ATTR: &[u8] = b"user.xdg.tags\0";

pub const COLORS: [(&str, Color32); 7] = [
    ("Red", Color32::from_rgb(0xe5, 0x48, 0x4d)),
    ("Orange", Color32::from_rgb(0xf0, 0x8a, 0x24)),
    ("Yellow", Color32::from_rgb(0xe8, 0xc1, 0x2f)),
    ("Green", Color32::from_rgb(0x46, 0xb3, 0x5c)),
    ("Blue", Color32::from_rgb(0x3b, 0x8e, 0xea)),
    ("Purple", Color32::from_rgb(0xa1, 0x5f, 0xd6)),
    ("Gray", Color32::from_rgb(0x8e, 0x8e, 0x93)),
];

/// The colour of a colour label ("red", "Red": any case).
pub fn color_of(tag: &str) -> Option<Color32> {
    COLORS.iter().find(|(n, _)| n.eq_ignore_ascii_case(tag)).map(|(_, c)| *c)
}

/// "Red,work, taxes" as tags, in their order, without blanks or repeats.
pub fn parse(v: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for t in v.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

fn cpath(p: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).ok()
}

/// A file's tags (none when it has no attribute or its filesystem has none).
pub fn read(p: &Path) -> Vec<String> {
    let Some(c) = cpath(p) else { return vec![] };
    let mut buf = [0u8; 1024];
    let n = unsafe { libc::getxattr(c.as_ptr(), ATTR.as_ptr().cast(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return vec![];
    }
    parse(&String::from_utf8_lossy(&buf[..n as usize]))
}

/// Give `p` exactly these tags (no tags removes the attribute).
pub fn write(p: &Path, tags: &[String]) -> std::io::Result<()> {
    let c = cpath(p).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let r = if tags.is_empty() {
        let r = unsafe { libc::removexattr(c.as_ptr(), ATTR.as_ptr().cast()) };
        if r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENODATA) { 0 } else { r }
    } else {
        let v = tags.join(",");
        unsafe { libc::setxattr(c.as_ptr(), ATTR.as_ptr().cast(), v.as_ptr().cast(), v.len(), 0) }
    };
    if r == 0 {
        set_cached(p, tags.to_vec());
        Ok(())
    } else {
        let e = std::io::Error::last_os_error();
        Err(if e.raw_os_error() == Some(libc::ENOTSUP) { std::io::Error::other("This drive can't keep tags") } else { e })
    }
}

/// Add or remove one tag on every file; the first failure is returned.
pub fn toggle(paths: &[PathBuf], tag: &str, on: bool) -> Result<(), String> {
    let mut first = None;
    for p in paths {
        let mut t = read(p);
        let had = t.iter().any(|x| x.eq_ignore_ascii_case(tag));
        if on && !had {
            t.push(tag.to_string());
        } else if !on && had {
            t.retain(|x| !x.eq_ignore_ascii_case(tag));
        } else {
            continue;
        }
        if let Err(e) = write(p, &t) {
            first.get_or_insert(format!("{}: {e}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    first.map_or(Ok(()), Err)
}

// ------------------------------------------------------------------ cache

#[derive(Default)]
struct Cache {
    /// Files known to have tags.
    tagged: HashMap<PathBuf, Vec<String>>,
    /// Files read and found to have none (hashes: 8 bytes each, for big folders).
    bare: HashSet<u64>,
    /// Asked for and not back yet.
    pending: HashSet<u64>,
    /// Bumped when anything lands, so views know to look again.
    generation: u64,
}

static CACHE: RwLock<Option<Cache>> = RwLock::new(None);
static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WORKER: Mutex<Option<mpsc::Sender<Vec<PathBuf>>>> = Mutex::new(None);

/// A path's hash, the same in every thread and call (unseeded).
fn key(p: &Path) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// What is known of `p`'s tags: Some (maybe empty) once read, None while not yet read
/// (it is then queued, for `flush` to send to the worker).
pub fn get<R>(p: &Path, f: impl FnOnce(&[String]) -> R) -> Option<R> {
    let k = key(p);
    {
        let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if let Some(t) = c.tagged.get(p) {
                return Some(f(t));
            }
            if c.bare.contains(&k) {
                return Some(f(&[]));
            }
            if c.pending.contains(&k) {
                return None;
            }
        }
    }
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Cache::default).pending.insert(k);
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(p.to_path_buf());
    None
}

fn set_cached(p: &Path, tags: Vec<String>) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(Cache::default);
    let k = key(p);
    c.pending.remove(&k);
    if tags.is_empty() {
        c.tagged.remove(p);
        c.bare.insert(k);
    } else {
        c.bare.remove(&k);
        c.tagged.insert(p.to_path_buf(), tags);
    }
    c.generation += 1;
}

/// Forget what is known of `p` (it changed on disk), so it is read again.
pub fn invalidate(p: &Path) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        let k = key(p);
        if c.tagged.remove(p).is_some() | c.bare.remove(&k) {
            c.generation += 1;
        }
    }
}

pub fn generation() -> u64 {
    CACHE.read().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |c| c.generation)
}

/// Every tag in use among the files read so far (for the menu), sorted.
pub fn known() -> Vec<String> {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<String> = g.as_ref().map(|c| c.tagged.values().flatten().filter(|t| color_of(t).is_none()).cloned().collect::<HashSet<_>>().into_iter().collect()).unwrap_or_default();
    v.sort_by_key(|t| t.to_lowercase());
    v
}

/// Send what was asked for this frame to the worker (started on first use).
pub fn flush(ctx: &egui::Context) {
    let batch = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if batch.is_empty() {
        return;
    }
    let mut w = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    let tx = w.get_or_insert_with(|| {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(batch) = rx.recv() {
                for chunk in batch.chunks(2000) {
                    let got: Vec<(PathBuf, Vec<String>)> = chunk.iter().map(|p| (p.clone(), read(p))).collect();
                    for (p, t) in got {
                        set_cached(&p, t);
                    }
                    ctx.request_repaint();
                }
            }
        });
        tx
    });
    let _ = tx.send(batch);
}

/// Every tag among the files read so far, colour labels included (for the sidebar).
pub fn in_use() -> HashSet<String> {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    g.as_ref().map(|c| c.tagged.values().flatten().cloned().collect()).unwrap_or_default()
}

/// Add the tags seen on files since to `known`; true when it grew.
pub fn remember(known: &mut Vec<String>) -> bool {
    let mut grew = false;
    for t in in_use() {
        if !known.iter().any(|k| k.eq_ignore_ascii_case(&t)) {
            known.push(t);
            grew = true;
        }
    }
    grew
}

/// Where a tag sorts: the colour labels in their fixed order, then every word.
pub fn rank(t: &str) -> u8 {
    COLORS.iter().position(|(n, _)| n.eq_ignore_ascii_case(t)).unwrap_or(COLORS.len()) as u8
}

/// The tag a file sorts and groups by: its first in the Tags order.
pub fn leading(t: &[String]) -> Option<&String> {
    t.iter().min_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())))
}

/// Known tags in the Tags order: colour labels first, then words A to Z.
pub fn ordered(known: &[String]) -> Vec<String> {
    let mut v = known.to_vec();
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())));
    v
}

/// Sort keys for these files by their leading tag (the order `leading` gives), None
/// for a file with none or not read yet. The words are ranked among themselves first,
/// so keys compare as numbers.
pub fn sort_keys<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<Option<u64>> {
    let lead: Vec<Option<(u8, String)>> = paths.map(|p| get(p, |t| leading(t).map(|l| (rank(l), l.to_lowercase()))).flatten()).collect();
    let mut words: Vec<&str> = lead.iter().flatten().filter(|l| l.0 as usize == COLORS.len()).map(|l| l.1.as_str()).collect();
    words.sort_unstable();
    words.dedup();
    lead.iter().map(|l| l.as_ref().map(|(r, w)| ((*r as u64) << 32) + if (*r as usize) < COLORS.len() { 0 } else { words.binary_search(&w.as_str()).unwrap_or(0) as u64 })).collect()
}

// ------------------------------------------------------------------ the files with a tag

/// Every file and folder under `root` carrying `tag` (in any case), each sent in a
/// batch to `found` (false stops). The same places are skipped as a smart folder skips:
/// hidden folders, build output and installed packages, links, other file systems.
/// Reading the attribute costs about a stat per file, so a few threads share one stack
/// of folders, and every tagged file met on the way is cached for the rows and the
/// sidebar.
pub fn search(root: &Path, tag: &str, found: &(dyn Fn(Vec<crate::fsx::Entry>) -> bool + Sync), cancel: &std::sync::atomic::AtomicBool, truncated: &std::sync::atomic::AtomicBool) {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return };
    let rb = root.as_os_str().as_encoded_bytes();
    let rel_at = (rb.len() + usize::from(rb.last() != Some(&b'/'))).clamp(1, u16::MAX as usize) as u16;
    // Folders still to read, and how many are being read (more may come from those).
    let work = Mutex::new((vec![root.to_path_buf()], 0usize));
    let cv = std::sync::Condvar::new();
    let total = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 4);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut batch = vec![];
                let mut flushed = std::time::Instant::now();
                loop {
                    let d = {
                        let mut g = work.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if cancel.load(Relaxed) {
                                return;
                            }
                            if let Some(d) = g.0.pop() {
                                g.1 += 1;
                                break Some(d);
                            }
                            if g.1 == 0 {
                                break None;
                            }
                            g = cv.wait_timeout(g, std::time::Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
                        }
                    };
                    let Some(d) = d else {
                        cv.notify_all();
                        if !batch.is_empty() {
                            found(batch);
                        }
                        return;
                    };
                    let mut subs = vec![];
                    if let Ok(rd) = std::fs::read_dir(&d) {
                        let kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
                        let has = |n: &str| kids.iter().any(|k| k.0 == n);
                        for (name, ft, path) in &kids {
                            if name.starts_with('.') || ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
                                continue;
                            }
                            if ft.is_dir() && crate::buildclean::kind_by_name(name, &has).is_none() && std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == dev) {
                                subs.push(path.clone());
                            }
                            let t = read(path);
                            if t.is_empty() {
                                continue;
                            }
                            let hit = t.iter().any(|x| x.eq_ignore_ascii_case(tag));
                            set_cached(path, t);
                            if let Some(mut e) = hit.then(|| crate::fsx::Entry::from_path(path)).flatten() {
                                e.rel_at = rel_at;
                                batch.push(e);
                                if total.fetch_add(1, Relaxed) + 1 >= crate::fsx::WALK_LIMIT {
                                    truncated.store(true, Relaxed);
                                    cancel.store(true, Relaxed);
                                }
                            }
                        }
                    }
                    {
                        let mut g = work.lock().unwrap_or_else(|e| e.into_inner());
                        g.0.extend(subs);
                        g.1 -= 1;
                    }
                    cv.notify_all();
                    if !batch.is_empty() && (batch.len() >= 1024 || flushed.elapsed().as_millis() > 50) {
                        if !found(std::mem::take(&mut batch)) {
                            cancel.store(true, Relaxed);
                        }
                        flushed = std::time::Instant::now();
                    }
                }
            });
        }
    });
}

// ------------------------------------------------------------------ changing tags, undoably

/// A file's tags before and after a change.
pub type Change = (PathBuf, Vec<String>, Vec<String>);

#[derive(Clone)]
pub enum How {
    Add(String),
    Remove(String),
    Rename(String, String),
}

/// The tags `t` becomes.
pub fn edited(t: &[String], how: &How) -> Vec<String> {
    let mut v: Vec<String> = t.to_vec();
    match how {
        How::Add(tag) if !v.iter().any(|x| x.eq_ignore_ascii_case(tag)) => v.push(tag.clone()),
        How::Add(_) => {}
        How::Remove(tag) => v.retain(|x| !x.eq_ignore_ascii_case(tag)),
        How::Rename(from, to) => {
            for x in v.iter_mut().filter(|x| x.eq_ignore_ascii_case(from)) {
                *x = to.clone();
            }
            v = parse(&v.join(","));
        }
    }
    v
}

pub enum Job {
    /// Change these files' tags.
    Edit(Vec<PathBuf>, How),
    /// Put changes back (true) or make them again, each only where the file's tags are
    /// still what the change left (or found).
    Restore(Vec<Change>, bool),
}

/// Where a finished change goes: a new one onto Undo, or one taken back or done again.
pub enum Record {
    New(String),
    Undid(String),
    Redid(String),
}

struct Done {
    record: Record,
    changes: Vec<Change>,
    failed: Vec<String>,
    /// A tag asked to go that no file turned out to have.
    gone: Option<String>,
}

/// The changes made: (path, before, after), in the original orientation for Restore.
fn run(job: Job) -> (Vec<Change>, Vec<String>) {
    let mut out = vec![];
    let mut failed = vec![];
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match job {
        Job::Edit(paths, how) => {
            for p in paths {
                let before = read(&p);
                let after = edited(&before, &how);
                if after == before {
                    continue;
                }
                match write(&p, &after) {
                    Ok(()) => out.push((p, before, after)),
                    Err(e) => failed.push(format!("{}: {e}", name(&p))),
                }
            }
        }
        Job::Restore(changes, back) => {
            for (p, before, after) in changes {
                let (now, to) = if back { (&after, &before) } else { (&before, &after) };
                if read(&p) != *now {
                    failed.push(format!("{}: its tags changed since", name(&p)));
                    continue;
                }
                match write(&p, to) {
                    Ok(()) => out.push((p, before, after)),
                    Err(e) => failed.push(format!("{}: {e}", name(&p))),
                }
            }
        }
    }
    (out, failed)
}

/// Finished on workers, by the thread whose app asked (tests run several apps at once).
static DONE: Mutex<Vec<(std::thread::ThreadId, Done)>> = Mutex::new(Vec::new());
/// The workers running, by the thread whose app started each.
static RUNNING: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());

fn running(me: std::thread::ThreadId, on: bool) {
    let mut g = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().position(|t| *t == me) {
        Some(i) if !on => {
            g.swap_remove(i);
        }
        _ if on => g.push(me),
        _ => {}
    }
}
/// Bulk changes waiting for their files to be found: (what, the files found).
static FOUND: Mutex<Vec<(std::thread::ThreadId, Bulk, Vec<PathBuf>)>> = Mutex::new(Vec::new());

/// A tag change is being made on a worker (Undo waits for it).
pub fn busy() -> bool {
    let me = std::thread::current().id();
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).contains(&me)
}

/// Change tags, recorded as one change for Undo. A few files are done now; more go to
/// a worker, a frame never waits on them.
pub fn start(app: &mut crate::app::App, job: Job, record: Record) {
    let n = match &job {
        Job::Edit(p, _) => p.len(),
        Job::Restore(c, _) => c.len(),
    };
    if n <= 200 {
        let (changes, failed) = run(job);
        finish(app, Done { record, changes, failed, gone: None });
        return;
    }
    let (ctx, me) = (app.ctx.clone(), std::thread::current().id());
    running(me, true);
    std::thread::spawn(move || {
        let (changes, failed) = run(job);
        DONE.lock().unwrap_or_else(|e| e.into_inner()).push((me, Done { record, changes, failed, gone: None }));
        running(me, false);
        ctx.request_repaint();
    });
}

fn finish(app: &mut crate::app::App, d: Done) {
    use crate::app::{UndoEntry, UndoKind};
    // The tags the change touched: one no file read since has any more leaves the sidebar.
    let touched: HashSet<String> = d.changes.iter().flat_map(|c| c.1.iter().chain(c.2.iter())).map(|t| t.to_lowercase()).chain(d.gone.iter().map(|t| t.to_lowercase())).collect();
    if !d.changes.is_empty() {
        let kind = UndoKind::Tagged(d.changes);
        match d.record {
            Record::New(label) => app.undo.push(UndoEntry { label, kind }),
            Record::Undid(label) => app.undo.push_undone(UndoEntry { label, kind }),
            Record::Redid(label) => app.undo.push_redone(UndoEntry { label, kind }),
        }
    }
    let live: HashSet<String> = in_use().iter().map(|t| t.to_lowercase()).collect();
    let before = app.ud.tags.len();
    app.ud.tags.retain(|t| !touched.contains(&t.to_lowercase()) || live.contains(&t.to_lowercase()));
    let grew = remember(&mut app.ud.tags);
    app.ud_dirty |= grew || app.ud.tags.len() != before;
    // Views of a tag find their files again.
    for t in app.panels.values_mut().flat_map(|p| p.tabs.iter_mut()) {
        if t.smart.as_ref().is_some_and(|s| touched.contains(&s.tag.to_lowercase())) {
            t.reload();
        }
    }
    match d.failed.as_slice() {
        [] => {}
        [one] => app.toast(format!("Couldn't change the tags of {one}")),
        [one, rest @ ..] => app.toast(format!("Couldn't change the tags of {one} and {} more", rest.len())),
    }
}

/// A change to a tag on every file in the home folder that has it.
pub enum Bulk {
    Rename(String, String),
    Remove(String),
}

/// Find the files first, on a worker; `pump` then asks (Remove) or goes ahead (Rename).
pub fn bulk(app: &mut crate::app::App, b: Bulk) {
    let tag = match &b {
        Bulk::Rename(t, _) | Bulk::Remove(t) => t.clone(),
    };
    app.toast(format!("Looking for “{tag}” in your home folder…"));
    let (ctx, me) = (app.ctx.clone(), std::thread::current().id());
    running(me, true);
    std::thread::spawn(move || {
        let paths = Mutex::new(vec![]);
        let (cancel, truncated) = (std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicBool::new(false));
        search(&crate::fsx::home(), &tag, &|batch| {
            paths.lock().unwrap_or_else(|e| e.into_inner()).extend(batch.into_iter().map(|e| e.path));
            true
        }, &cancel, &truncated);
        let paths = paths.into_inner().unwrap_or_else(|e| e.into_inner());
        FOUND.lock().unwrap_or_else(|e| e.into_inner()).push((me, b, paths));
        running(me, false);
        ctx.request_repaint();
    });
}

/// Take in what the workers finished (once a frame).
pub fn pump(app: &mut crate::app::App) {
    let me = std::thread::current().id();
    let done: Vec<Done> = {
        let mut g = DONE.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest) = std::mem::take(&mut *g).into_iter().partition(|d| d.0 == me);
        *g = rest;
        mine.into_iter().map(|d: (std::thread::ThreadId, Done)| d.1).collect()
    };
    for d in done {
        finish(app, d);
    }
    let found: Vec<(Bulk, Vec<PathBuf>)> = {
        let mut g = FOUND.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest) = std::mem::take(&mut *g).into_iter().partition(|d| d.0 == me);
        *g = rest;
        mine.into_iter().map(|d: (std::thread::ThreadId, Bulk, Vec<PathBuf>)| (d.1, d.2)).collect()
    };
    for (b, paths) in found {
        let n = paths.len();
        let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        match b {
            Bulk::Remove(tag) | Bulk::Rename(tag, _) if n == 0 => {
                app.toast(format!("No file in your home folder has “{tag}”"));
                finish(app, Done { record: Record::New(String::new()), changes: vec![], failed: vec![], gone: Some(tag) });
            }
            Bulk::Remove(tag) => {
                // Found: the question says how many, the toast saying it was looking goes.
                app.toast = None;
                let label = format!("Remove tag “{tag}”");
                app.popup = crate::popups::Popup::confirm(format!("Remove “{tag}” from {files} in your home folder?"), "Remove", Box::new(move |app: &mut crate::app::App| start(app, Job::Edit(paths, How::Remove(tag)), Record::New(label))));
            }
            Bulk::Rename(from, to) => {
                app.toast(format!("Renaming “{from}” on {files}"));
                start(app, Job::Edit(paths, How::Rename(from.clone(), to.clone())), Record::New(format!("Rename tag “{from}” to “{to}”")));
            }
        }
    }
}
