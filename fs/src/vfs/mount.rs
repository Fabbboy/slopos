use crate::vfs::traits::{FileSystem, VfsError, VfsResult};
use slopos_ostd::lock_class;
use slopos_ostd::sync::{IrqRwLock, LOCK_LEVEL_REGISTRY};

use crate::MAX_PATH_LEN;

pub const MAX_MOUNTS: usize = 16;

pub struct MountPoint {
    path: [u8; MAX_PATH_LEN],
    path_len: usize,
    fs: Option<&'static dyn FileSystem>,
    flags: u32,
}

impl MountPoint {
    const fn empty() -> Self {
        Self {
            path: [0; MAX_PATH_LEN],
            path_len: 0,
            fs: None,
            flags: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.fs.is_some()
    }

    fn path_bytes(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

pub struct MountTable {
    mounts: [MountPoint; MAX_MOUNTS],
    count: usize,
}

impl MountTable {
    const fn new() -> Self {
        Self {
            mounts: [const { MountPoint::empty() }; MAX_MOUNTS],
            count: 0,
        }
    }

    pub fn mount(&mut self, path: &[u8], fs: &'static dyn FileSystem, flags: u32) -> VfsResult<()> {
        if path.is_empty() || path[0] != b'/' {
            return Err(VfsError::InvalidPath);
        }
        if path.len() > MAX_PATH_LEN {
            return Err(VfsError::NameTooLong);
        }

        for mp in self.mounts.iter() {
            if mp.is_active() && mp.path_len == path.len() && &mp.path[..path.len()] == path {
                return Err(VfsError::AlreadyExists);
            }
        }

        let slot = self
            .mounts
            .iter_mut()
            .find(|m| !m.is_active())
            .ok_or(VfsError::NoSpace)?;

        slot.path[..path.len()].copy_from_slice(path);
        slot.path_len = path.len();
        slot.fs = Some(fs);
        slot.flags = flags;
        self.count += 1;

        Ok(())
    }

    pub fn unmount(&mut self, path: &[u8]) -> VfsResult<()> {
        for mp in self.mounts.iter_mut() {
            if mp.is_active() && mp.path_len == path.len() && &mp.path[..path.len()] == path {
                mp.fs = None;
                mp.path_len = 0;
                self.count -= 1;
                return Ok(());
            }
        }
        Err(VfsError::NotFound)
    }

    pub fn resolve(&self, path: &[u8]) -> VfsResult<(&'static dyn FileSystem, usize)> {
        if path.is_empty() || path[0] != b'/' {
            return Err(VfsError::InvalidPath);
        }

        let mut best_match: Option<(&MountPoint, usize)> = None;

        for mp in self.mounts.iter() {
            if !mp.is_active() {
                continue;
            }

            let mp_path = mp.path_bytes();

            let matches = if mp_path == b"/" {
                true
            } else if path.len() >= mp_path.len() {
                let prefix_matches = &path[..mp_path.len()] == mp_path;
                let boundary_ok =
                    path.len() == mp_path.len() || path.get(mp_path.len()) == Some(&b'/');
                prefix_matches && boundary_ok
            } else {
                false
            };

            if matches {
                let match_len = mp_path.len();
                if best_match.map_or(true, |(_, len)| match_len > len) {
                    best_match = Some((mp, match_len));
                }
            }
        }

        let (mp, match_len) = best_match.ok_or(VfsError::NotFound)?;
        let fs = mp.fs.ok_or(VfsError::NotFound)?;

        Ok((fs, match_len))
    }

    pub fn mount_count(&self) -> usize {
        self.count
    }

    /// Once per mount point, so a filesystem mounted twice is visited twice.
    pub fn for_each_mount(&self, callback: &mut dyn FnMut(&'static dyn FileSystem)) {
        for mp in self.mounts.iter() {
            if let Some(fs) = mp.fs {
                callback(fs);
            }
        }
    }

    /// Iterate over mount points that are direct children of `parent_path`,
    /// calling `callback` with the child's name component (`b"tmp"` when parent
    /// is `b"/"` and the mount is `b"/tmp"`). Returns the number visited.
    pub fn for_each_child_mount(
        &self,
        parent_path: &[u8],
        callback: &mut dyn FnMut(&[u8]) -> bool,
    ) -> usize {
        let plen = {
            let mut len = parent_path.len();
            while len > 1 && parent_path[len - 1] == b'/' {
                len -= 1;
            }
            len
        };
        let parent = &parent_path[..plen];

        let mut count = 0;
        for mp in self.mounts.iter() {
            if !mp.is_active() {
                continue;
            }
            let mp_path = mp.path_bytes();

            if mp_path.len() == plen && &mp_path[..plen] == parent {
                continue;
            }

            let child_start = if parent == b"/" {
                if mp_path.len() <= 1 || mp_path[0] != b'/' {
                    continue;
                }
                1
            } else {
                if mp_path.len() <= plen + 1 || &mp_path[..plen] != parent || mp_path[plen] != b'/'
                {
                    continue;
                }
                plen + 1
            };

            let child_part = &mp_path[child_start..];

            if child_part.is_empty() || child_part.contains(&b'/') {
                continue;
            }

            if !callback(child_part) {
                break;
            }
            count += 1;
        }
        count
    }
}

static MOUNT_TABLE: IrqRwLock<MountTable> = IrqRwLock::new(
    MountTable::new(),
    lock_class!("MOUNT_TABLE", LOCK_LEVEL_REGISTRY),
);

pub fn mount(path: &[u8], fs: &'static dyn FileSystem, flags: u32) -> VfsResult<()> {
    MOUNT_TABLE.write().mount(path, fs, flags)
}

pub fn unmount(path: &[u8]) -> VfsResult<()> {
    MOUNT_TABLE.write().unmount(path)
}

pub fn with_mount_table<R>(f: impl FnOnce(&MountTable) -> R) -> R {
    let guard = MOUNT_TABLE.read();
    f(&guard)
}

/// The filesystem mounted **exactly** at `path`, if one is.
///
/// A per-component walk asks this at each step, so a mount crossed mid-path is
/// honoured rather than resolved inside the filesystem underneath it.
pub fn mount_at(path: &[u8]) -> Option<&'static dyn FileSystem> {
    let guard = MOUNT_TABLE.read();
    guard
        .mounts
        .iter()
        .find(|mp| mp.is_active() && mp.path_bytes() == path)
        .and_then(|mp| mp.fs)
}

pub fn resolve_mount<'a>(path: &'a [u8]) -> VfsResult<(&'static dyn FileSystem, &'a [u8])> {
    let (fs, match_len) = {
        let guard = MOUNT_TABLE.read();
        guard.resolve(path)?
    };

    let relative = if match_len >= path.len() {
        b"/" as &[u8]
    } else {
        &path[match_len..]
    };

    Ok((fs, relative))
}
