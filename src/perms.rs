//! Permissions and owners changed for several items at once, and all the way down.

use std::path::{Path, PathBuf};

/// What a change does to each item: mode bits turned on and off, and a new owner or group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Change {
    pub set: u32,
    pub clear: u32,
    pub owner: Option<u32>,
    pub group: Option<u32>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.set == 0 && self.clear == 0 && self.owner.is_none() && self.group.is_none()
    }

    /// The mode `old` becomes. A file inside a folder changed all the way down gets execute
    /// only where it had some already (chmod's X): documents don't turn into programs.
    pub fn mode(&self, old: u32, inner_file: bool) -> u32 {
        let set = if inner_file && old & 0o111 == 0 { self.set & !0o111 } else { self.set };
        (old & !self.clear) | set
    }

    /// The same change as pkexec runs it: chown's owner and chmod's symbolic mode.
    pub fn admin(&self, paths: Vec<PathBuf>, deep: bool) -> Option<crate::admin::Task> {
        let owner = (self.owner.is_some() || self.group.is_some()).then(|| format!("{}:{}", self.owner.map(user_name).unwrap_or_default(), self.group.map(group_name).unwrap_or_default()));
        let owner = owner.map(|o| o.trim_end_matches(':').to_string());
        let mode = self.symbolic(deep);
        (owner.is_some() || mode.is_some()).then_some(crate::admin::Task::Perms { paths, owner, mode, deep })
    }

    /// "u+rwX,g-w,o-rwx,u+s": what is set and cleared, for chmod (X with `deep`).
    pub fn symbolic(&self, deep: bool) -> Option<String> {
        let mut parts = vec![];
        for (who, shift, special) in [('u', 6, 0o4000), ('g', 3, 0o2000), ('o', 0, 0o1000)] {
            for (sign, bits) in [('+', self.set), ('-', self.clear)] {
                let mut s = String::new();
                for (bit, c) in [(4, 'r'), (2, 'w'), (1, if deep { 'X' } else { 'x' })] {
                    if bits >> shift & bit != 0 {
                        s.push(c);
                    }
                }
                if bits & special != 0 {
                    s.push(if who == 'o' { 't' } else { 's' });
                }
                if !s.is_empty() {
                    parts.push(format!("{who}{sign}{s}"));
                }
            }
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }
}

/// Apply `c` to `top` (and everything inside it when `deep`); what refused, and why.
pub fn apply(top: &Path, c: &Change, deep: bool, gate: &crate::ops::Gate) -> Vec<(PathBuf, std::io::Error)> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut errs = vec![];
    let mut stack = vec![(top.to_path_buf(), false)];
    while let Some((p, inner)) = stack.pop() {
        if gate.cancelled() {
            break;
        }
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if c.owner.is_some() || c.group.is_some() {
            let cp = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())).unwrap_or_default();
            if unsafe { libc::lchown(cp.as_ptr(), c.owner.unwrap_or(u32::MAX), c.group.unwrap_or(u32::MAX)) } != 0 {
                errs.push((p.clone(), std::io::Error::last_os_error()));
            }
        }
        // A link's own mode means nothing; what it points to is changed where it lives.
        if !m.is_symlink() && (c.set | c.clear) != 0 {
            let mode = c.mode(m.mode() & 0o7777, inner && !m.is_dir());
            if mode != m.mode() & 0o7777 {
                if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)) {
                    errs.push((p.clone(), e));
                }
            }
        }
        if deep && m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| (e.path(), true)));
            }
        }
    }
    errs
}

/// (id, name) of the people who log in (and root), and of every group, as /etc lists them.
pub fn users() -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = read_ids("/etc/passwd").into_iter().filter(|(id, _)| *id == 0 || (1000..60000).contains(id)).collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

pub fn groups() -> Vec<(u32, String)> {
    let mut v = read_ids("/etc/group");
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

pub(crate) fn read_ids(file: &str) -> Vec<(u32, String)> {
    std::fs::read_to_string(file).unwrap_or_default().lines().filter_map(|l| {
        let mut it = l.split(':');
        let name = it.next()?.to_string();
        it.next();
        Some((it.next()?.parse().ok()?, name))
    }).collect()
}

pub fn user_name(id: u32) -> String {
    read_ids("/etc/passwd").into_iter().find(|u| u.0 == id).map_or_else(|| id.to_string(), |u| u.1)
}

pub fn group_name(id: u32) -> String {
    read_ids("/etc/group").into_iter().find(|g| g.0 == id).map_or_else(|| id.to_string(), |g| g.1)
}
