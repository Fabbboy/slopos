use crate::vfs::traits::{FileSystem, VfsError, VfsResult};
use slopos_lib::IrqRwLock;

const MAX_MOUNTS: usize = 16;
const MAX_PATH_LEN: usize = 256;

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

    pub fn resolve<'a>(&self, path: &'a [u8]) -> VfsResult<(&'static dyn FileSystem, &'a [u8])> {
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

        let relative = if match_len >= path.len() {
            b"/" as &[u8]
        } else if path[match_len] == b'/' {
            &path[match_len..]
        } else {
            &path[match_len..]
        };

        Ok((fs, relative))
    }

    pub fn mount_count(&self) -> usize {
        self.count
    }
}

static MOUNT_TABLE: IrqRwLock<MountTable> = IrqRwLock::new(MountTable::new());

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

pub fn resolve_mount(path: &[u8]) -> VfsResult<(&'static dyn FileSystem, &'static [u8])> {
    let guard = MOUNT_TABLE.read();
    let (fs, relative) = guard.resolve(path)?;
    let relative_static: &'static [u8] =
        unsafe { core::slice::from_raw_parts(relative.as_ptr(), relative.len()) };
    Ok((fs, relative_static))
}
