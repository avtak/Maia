//! Redo: a change Undo took back, done again. Each kind checks first that things are
//! where Undo left them, as Undo does, and does again only what it finds so; what it did
//! goes back on Undo's list.

use std::path::{Path, PathBuf};

use crate::app::{App, UndoEntry, UndoKind, UndoStack};
use crate::ops::{self, OpKind};

/// A move handed to a job by Undo or Redo, and the change it is: what to record once the
/// job ends with what it managed.
pub enum Step {
    /// Undo moving things back: done again by Redo.
    Undid { label: String, over: bool },
    /// Redo moving them again: taken back by Undo.
    Redid { label: String, over: bool },
}

/// Does any of these moves (from, to) cross to another file system? Those are copies,
/// and go to a job with its card rather than happen on a frame.
pub fn crosses<'a>(mut moves: impl Iterator<Item = (&'a Path, &'a Path)>) -> bool {
    moves.any(|(from, to)| ops::dev_of(from) != to.parent().and_then(ops::dev_of))
}

/// "Moving notes.txt back", "Moving 3 items".
pub fn place_label(pairs: &[(PathBuf, PathBuf)], back: bool) -> String {
    let what = match pairs {
        [(one, _)] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        v => format!("{} items", v.len()),
    };
    format!("Moving {what}{}", if back { " back" } else { "" })
}

/// What a job of Undo or Redo moved, onto the list it belongs to.
pub fn placed(undo: &mut UndoStack, step: Step, p: &ops::Progress) {
    match step {
        Step::Undid { label, over } => {
            // Moved from where the change put them to where they were: Redo moves them on again.
            let pairs: Vec<(PathBuf, PathBuf)> = p.produced.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
            if !pairs.is_empty() || !p.restored.is_empty() {
                let kind = if over { UndoKind::MovedOver(pairs, p.restored.clone()) } else { UndoKind::Move(pairs) };
                undo.push_undone(UndoEntry { label, kind });
            }
        }
        Step::Redid { label, over } => {
            if !p.produced.is_empty() {
                let kind = if over { UndoKind::MovedOver(p.produced.clone(), p.replaced.clone()) } else { UndoKind::Move(p.produced.clone()) };
                undo.push_redone(UndoEntry { label, kind });
            }
        }
    }
}

fn here(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Each to the trash; those that went.
pub fn trash_each<'a>(paths: impl Iterator<Item = &'a PathBuf>, errs: &mut usize) -> Vec<PathBuf> {
    paths
        .filter(|p| {
            let ok = crate::trashcan::delete(p).is_ok();
            *errs += !ok as usize;
            ok
        })
        .cloned()
        .collect()
}

/// Each back from the trash into its own place, where that is free; those that came.
pub fn untrash_each(paths: &[PathBuf], errs: &mut usize) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| {
            let ok = !here(p) && crate::picfix::newest_trashed(p).is_some_and(|it| crate::trashcan::restore(&it).is_ok());
            *errs += !ok as usize;
            ok
        })
        .cloned()
        .collect()
}

/// Each swapped with its latest version in the trash; those swapped.
pub fn swap_each(paths: &[PathBuf], errs: &mut usize) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| {
            let ok = crate::picfix::undo_replaced(std::slice::from_ref(*p)) == 0;
            *errs += !ok as usize;
            ok
        })
        .cloned()
        .collect()
}

/// `swap_each`, with the version trashed by `by` (when the change, or its Undo, was
/// made): one trashed since belongs to a later change.
pub fn swap_each_by(paths: &[PathBuf], by: Option<std::time::SystemTime>, errs: &mut usize) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| {
            let ok = crate::picfix::undo_replaced_by(std::slice::from_ref(*p), by) == 0;
            *errs += !ok as usize;
            ok
        })
        .cloned()
        .collect()
}

impl App {
    /// Do again the change Undo took back last; false when some of it couldn't be.
    pub fn redo_last(&mut self) -> bool {
        if !self.job_steps.is_empty() {
            self.toast("Redo waits for the move under way");
            return false;
        }
        if crate::tags::busy() {
            self.toast("Redo waits for the tags being changed");
            return false;
        }
        let undone = self.undo.redos().len().checked_sub(1).and_then(|i| self.undo.undone_when(i));
        let Some(u) = self.undo.pop_redo() else {
            self.toast("Nothing to redo");
            return true;
        };
        let mut errs = 0;
        let mut changed = ops::Changed::default();
        let did = match &u.kind {
            UndoKind::Tagged(ch) => {
                self.toast(format!("Redid “{}”", u.label));
                crate::tags::start(self, crate::tags::Job::Restore(ch.clone(), false), crate::tags::Record::Redid(u.label.clone()));
                return true;
            }
            UndoKind::Rename(pairs) => {
                let plan: Vec<(PathBuf, String)> = pairs.iter().map(|(from, to)| (from.clone(), to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())).collect();
                errs = crate::rename::apply(&plan).len();
                let done: Vec<(PathBuf, PathBuf)> = pairs.iter().filter(|(from, to)| here(to) && !here(from)).cloned().collect();
                changed = ops::Changed::moved(&done);
                UndoKind::Rename(done)
            }
            UndoKind::Move(pairs) | UndoKind::MovedOver(pairs, _) => {
                let (over, replace) = match &u.kind {
                    UndoKind::MovedOver(_, r) => (true, r.clone()),
                    _ => (false, vec![]),
                };
                if crosses(pairs.iter().map(|(s, d)| (s.as_path(), d.as_path()))) {
                    let label = place_label(pairs, false);
                    let id = self.alloc_id();
                    self.job_steps.insert(id, Step::Redid { label: u.label.clone(), over });
                    self.jobs.push(ops::spawn_in(id, OpKind::Place { pairs: pairs.clone(), over: replace, back: vec![], label }, self.ctx.clone(), &self.queue, self.cfg.check_copies));
                    return true;
                }
                let (mut done, mut replaced) = (vec![], vec![]);
                for (s, d) in pairs {
                    let taken = here(d);
                    // Only what the move replaced the first time may be replaced again.
                    if !here(s) || (taken && !replace.contains(d)) || (taken && crate::trashcan::delete(d).is_err()) {
                        errs += 1;
                        continue;
                    }
                    match crate::fsx::rename_new(s, d) {
                        Ok(()) => {
                            done.push((s.clone(), d.clone()));
                            if taken {
                                replaced.push(d.clone());
                            }
                        }
                        Err(_) => {
                            errs += 1;
                            if taken && let Some(it) = crate::picfix::newest_trashed(d) {
                                let _ = crate::trashcan::restore(&it);
                            }
                        }
                    }
                }
                changed = ops::Changed::moved(&done);
                if over { UndoKind::MovedOver(done, replaced) } else { UndoKind::Move(done) }
            }
            UndoKind::Created(paths) => {
                let back = untrash_each(paths, &mut errs);
                changed = ops::Changed::at(&back, &[]);
                UndoKind::Created(back)
            }
            UndoKind::Copied(made, replaced) => {
                let back = untrash_each(&made.iter().filter(|p| !replaced.contains(p)).cloned().collect::<Vec<_>>(), &mut errs);
                let swapped = swap_each(replaced, &mut errs);
                changed = ops::Changed::at(&back, &[]);
                UndoKind::Copied(back.into_iter().chain(swapped.iter().cloned()).collect(), swapped)
            }
            UndoKind::Synced(made, replaced, removed) => {
                let back = untrash_each(&made.iter().filter(|p| !replaced.contains(p)).cloned().collect::<Vec<_>>(), &mut errs);
                let swapped = swap_each(replaced, &mut errs);
                errs += removed.iter().filter(|p| !here(p)).count();
                let gone = trash_each(removed.iter().filter(|p| here(p)), &mut errs);
                changed = ops::Changed::at(&back, &gone);
                UndoKind::Synced(back.into_iter().chain(swapped.iter().cloned()).collect(), swapped, gone)
            }
            UndoKind::Trashed(orig) => {
                let missing = orig.iter().filter(|p| !here(p)).count();
                let gone = trash_each(orig.iter().filter(|p| here(p)), &mut errs);
                errs += missing;
                changed = ops::Changed::at(&[], &gone);
                UndoKind::Trashed(gone)
            }
            UndoKind::Replaced(paths) => UndoKind::Replaced(swap_each_by(paths, undone, &mut errs)),
            UndoKind::Sorted(pairs, made) => {
                // The folders again, outermost first, then each file into its own.
                let mut new = vec![];
                for d in made {
                    if std::fs::create_dir(d).is_ok() {
                        new.push(d.clone());
                    }
                }
                let mut done = vec![];
                for (s, d) in pairs {
                    if !here(s) || crate::fsx::rename_new(s, d).is_err() {
                        errs += 1;
                    } else {
                        done.push((s.clone(), d.clone()));
                    }
                }
                changed = ops::Changed::moved(&done);
                UndoKind::Sorted(done, new)
            }
        };
        if did.count() > 0 {
            self.undo.push_redone(UndoEntry { label: u.label.clone(), kind: did });
        }
        if changed.is_empty() {
            for p in self.panels.values_mut() {
                p.tab_mut().reload();
            }
        } else {
            self.show_changed(&changed);
        }
        if errs > 0 {
            self.toast(format!("Redo “{}” partly failed", u.label));
        } else {
            self.toast(format!("Redid “{}”", u.label));
        }
        errs == 0
    }
}
