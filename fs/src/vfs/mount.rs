use crate::vfs::traits::{FileSystem, VfsError, VfsResult};
use slopos_sync::{IrqRwLock, LOCK_LEVEL_REGISTRY};

use crate::MAX_PATH_LEN;

const MAX_MOUNTS: usize = 16;

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

    /// Iterate over mount points that are direct children of `parent_path`.
    ///
    /// For each child mount, calls `callback` with the child's name component
    /// (e.g., `b"tmp"` when parent is `b"/"` and mount is `b"/tmp"`).
    /// Returns the number of children visited. This is the kernel-level
    /// mechanism that lets directory listings include mounted sub-filesystems,
    /// mirroring how Linux VFS synthesises mount-point entries in readdir.
    pub fn for_each_child_mount(
        &self,
        parent_path: &[u8],
        callback: &mut dyn FnMut(&[u8]) -> bool,
    ) -> usize {
        // Normalise parent: strip trailing slashes (keep root "/")
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

            // Skip the mount at the parent path itself
            if mp_path.len() == plen && &mp_path[..plen] == parent {
                continue;
            }

            // Determine where the child component starts
            let child_start = if parent == b"/" {
                // Root parent: child mounts look like "/X"
                if mp_path.len() <= 1 || mp_path[0] != b'/' {
                    continue;
                }
                1
            } else {
                // Non-root parent: child mounts look like "<parent>/X"
                if mp_path.len() <= plen + 1 || &mp_path[..plen] != parent || mp_path[plen] != b'/'
                {
                    continue;
                }
                plen + 1
            };

            let child_part = &mp_path[child_start..];

            // Must be a single path component (no further slashes)
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

static MOUNT_TABLE: IrqRwLock<MountTable> = IrqRwLock::new(MountTable::new(), LOCK_LEVEL_REGISTRY);

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
