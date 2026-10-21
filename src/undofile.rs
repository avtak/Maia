//! Undo and Redo across restarts: both lists in Maia's state folder, written on a thread
//! of their own whenever they change and read back on one at start, where a change whose
//! files are all gone by then is left out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crate::app::{UndoEntry, UndoKind, UndoStack};

/// The most changes each list holds.
pub const MOST: usize = 100;
/// How old a change may be when Maia starts again.
pub const DAYS: u64 = 30;
/// How many paths the kept changes may name together: a move of a whole flattened tree
/// names hundreds of thousands, and the file would take seconds to read back.
pub const PATHS: usize = 100_000;

pub fn path() -> PathBuf {
    crate::config::state_dir().join("undo.json")
}

/// Both lists, each oldest first, with when each change was made (or undone).
#[derive(Default)]
pub struct Snapshot {
    pub undo: Vec<(Arc<UndoEntry>, SystemTime)>,
    pub redo: Vec<(Arc<UndoEntry>, SystemTime)>,
}

/// The oldest changes past `MOST` go.
pub fn cap(items: &mut Vec<Arc<UndoEntry>>, at: &mut Vec<SystemTime>) {
    if items.len() > MOST {
        let n = items.len() - MOST;
        items.drain(..n);
        at.drain(..n.min(at.len()));
    }
}

#[derive(serde::Serialize)]
struct Out<'a> {
    at: u64,
    label: &'a str,
    kind: &'a UndoKind,
}

#[derive(serde::Deserialize)]
struct In {
    at: u64,
    label: String,
    kind: UndoKind,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Saved {
    undo: Vec<serde_json::Value>,
    redo: Vec<serde_json::Value>,
}

impl UndoKind {
    fn paths(&self) -> usize {
        match self {
            UndoKind::Synced(a, b, c) => a.len() + b.len() + c.len(),
            UndoKind::Copied(a, b) => a.len() + b.len(),
            UndoKind::Sorted(v, m) => v.len() + m.len(),
            UndoKind::MovedOver(v, r) => v.len() + r.len(),
            k => k.count(),
        }
    }
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The file's text: the newest changes that fit the limits. A change naming a path that
/// isn't UTF-8 can't be written as JSON and stays in this run only.
fn encode(s: &Snapshot) -> Vec<u8> {
    let oldest = secs(SystemTime::now()).saturating_sub(DAYS * 86_400);
    let mut room = PATHS;
    let mut list = |v: &[(Arc<UndoEntry>, SystemTime)]| -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = v
            .iter()
            .rev()
            .take(MOST)
            .take_while(|(e, t)| secs(*t) >= oldest && room.checked_sub(e.kind.paths()).inspect(|r| room = *r).is_some())
            .filter_map(|(e, t)| serde_json::to_value(Out { at: secs(*t), label: &e.label, kind: &e.kind }).ok())
            .collect();
        out.reverse();
        out
    };
    let undo = list(&s.undo);
    let redo = list(&s.redo);
    serde_json::to_vec(&serde_json::json!({ "undo": undo, "redo": redo })).unwrap_or_default()
}

fn write(path: &Path, s: &Snapshot) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&encode(s))?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// The lists as the file has them, without changes past their age and those with nothing
/// left to act on: a move whose files are all gone, a trash emptied since.
fn read(path: &Path) -> Snapshot {
    let Ok(bytes) = std::fs::read(path) else { return Snapshot::default() };
    let saved: Saved = serde_json::from_slice(&bytes).unwrap_or_default();
    let oldest = secs(SystemTime::now()).saturating_sub(DAYS * 86_400);
    let trashed: HashSet<PathBuf> = crate::trashcan::list().into_iter().map(|it| it.original).collect();
    let list = |v: Vec<serde_json::Value>, undo: bool| -> Vec<(Arc<UndoEntry>, SystemTime)> {
        let mut out: Vec<_> = v
            .into_iter()
            .filter_map(|x| serde_json::from_value::<In>(x).ok())
            .filter(|e| e.at >= oldest && alive(&e.kind, undo, &trashed))
            .map(|e| (Arc::new(UndoEntry { label: e.label, kind: e.kind }), SystemTime::UNIX_EPOCH + Duration::from_secs(e.at)))
            .collect();
        if out.len() > MOST {
            out.drain(..out.len() - MOST);
        }
        out
    };
    Snapshot { undo: list(saved.undo, true), redo: list(saved.redo, false) }
}

/// Whether Undo (`undo`) or Redo has something to act on still: one of the paths it
/// would move, take or bring back is where it would look for it.
fn alive(k: &UndoKind, undo: bool, trashed: &HashSet<PathBuf>) -> bool {
    let here = |p: &PathBuf| std::fs::symlink_metadata(p).is_ok();
    let binned = |p: &PathBuf| trashed.contains(p);
    // Undo takes away what is here; Redo brings it back from the trash.
    let made = |v: &[PathBuf]| v.iter().any(|p| if undo { here(p) } else { binned(p) });
    match k {
        UndoKind::Rename(v) | UndoKind::Move(v) | UndoKind::Sorted(v, _) | UndoKind::MovedOver(v, _) => v.iter().any(|(a, b)| here(if undo { b } else { a })),
        UndoKind::Created(v) | UndoKind::Copied(v, _) => made(v),
        UndoKind::Synced(v, _, removed) => made(v) || removed.iter().any(|p| if undo { binned(p) } else { here(p) }),
        UndoKind::Trashed(v) => v.iter().any(|p| if undo { binned(p) } else { here(p) }),
        UndoKind::Replaced(v) => v.iter().any(|p| here(p) && binned(p)),
        UndoKind::Tagged(v) => v.iter().any(|c| here(&c.0)),
    }
}

/// Keeps the lists in their file: read once at start, written after each change.
#[derive(Default)]
pub struct Keeper {
    path: Option<PathBuf>,
    loading: Option<std::sync::mpsc::Receiver<Snapshot>>,
    saved: u64,
    writing: Arc<AtomicBool>,
}

impl Keeper {
    /// None keeps nothing: test builds, which share one home, unless a test asks.
    pub fn new(path: Option<PathBuf>, ctx: &egui::Context) -> Keeper {
        let loading = path.clone().map(|p| {
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(read(&p));
                ctx.request_repaint();
            });
            rx
        });
        Keeper { path, loading, saved: 0, writing: Default::default() }
    }

    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Read, and every change written.
    pub fn settled(&self, stack: &UndoStack) -> bool {
        self.loading.is_none() && stack.generation == self.saved && !self.writing.load(Ordering::Acquire)
    }

    /// Takes in what was read, and starts a write when the lists changed since the last.
    pub fn pump(&mut self, stack: &mut UndoStack, ctx: &egui::Context) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(s) => {
                    stack.restore(s);
                    self.saved = stack.generation;
                    self.loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(_) => self.loading = None,
            }
        }
        let Some(path) = &self.path else { return };
        if stack.generation == self.saved {
            return;
        }
        if self.writing.swap(true, Ordering::AcqRel) {
            // One write at a time: the change is written once the one before is.
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        self.saved = stack.generation;
        let (snap, path, writing) = (stack.snapshot(), path.clone(), self.writing.clone());
        std::thread::spawn(move || {
            if let Err(e) = write(&path, &snap) {
                eprintln!("maia: could not keep Undo in {}: {e}", path.display());
            }
            writing.store(false, Ordering::Release);
        });
    }

    /// At exit: the last change written before the process ends.
    pub fn flush(&mut self, stack: &UndoStack) {
        if let (Some(path), None) = (&self.path, &self.loading) {
            while self.writing.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if stack.generation != self.saved {
                let _ = write(path, &stack.snapshot());
                self.saved = stack.generation;
            }
        }
    }
}
