use crate::vfs::canon::canonicalise;
use crate::vfs::traits::{FileSystem, VfsError, VfsResult};
use slopos_ostd::lock_class;
use slopos_ostd::sync::{IrqRwLock, LOCK_LEVEL_REGISTRY};

use crate::MAX_PATH_LEN;

pub const MAX_MOUNTS: usize = 16;

/// Every mutation through this mount fails with `EROFS` at the VFS, before
/// the filesystem sees it.
pub const MOUNT_RDONLY: u32 = 1 << 0;

#[derive(Clone, Copy)]
pub struct Mounted {
    pub fs: &'static dyn FileSystem,
    pub flags: u32,
    /// Identity of the *mount*, not of the filesystem: one filesystem mounted
    /// at two paths carries two ids.
    pub id: u32,
}

impl Mounted {
    pub fn read_only(&self) -> bool {
        self.flags & MOUNT_RDONLY != 0
    }
}

pub struct MountPoint {
    path: [u8; MAX_PATH_LEN],
    path_len: usize,
    fs: Option<&'static dyn FileSystem>,
    flags: u32,
    id: u32,
}

impl MountPoint {
    const fn empty() -> Self {
        Self {
            path: [0; MAX_PATH_LEN],
            path_len: 0,
            fs: None,
            flags: 0,
            id: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.fs.is_some()
    }

    fn path_bytes(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut len = path.len();
    while len > 1 && path[len - 1] == b'/' {
        len -= 1;
    }
    &path[..len]
}

/// The trailing component of `mp_path` when it is a *direct* child of
/// `parent` — `b"tmp"` for parent `b"/"` and mount `b"/tmp"`. `None` when
/// `mp_path` is the parent itself, a deeper descendant, or unrelated.
fn child_component<'a>(parent: &[u8], mp_path: &'a [u8]) -> Option<&'a [u8]> {
    let start = if parent == b"/" {
        if mp_path.len() <= 1 || mp_path[0] != b'/' {
            return None;
        }
        1
    } else {
        if mp_path.len() <= parent.len() + 1
            || &mp_path[..parent.len()] != parent
            || mp_path[parent.len()] != b'/'
        {
            return None;
        }
        parent.len() + 1
    };

    let child = &mp_path[start..];
    if child.is_empty() || child.contains(&b'/') {
        return None;
    }
    Some(child)
}

pub struct MountTable {
    mounts: [MountPoint; MAX_MOUNTS],
    count: usize,
    next_id: u32,
}

impl MountTable {
    const fn new() -> Self {
        Self {
            mounts: [const { MountPoint::empty() }; MAX_MOUNTS],
            count: 0,
            next_id: 1,
        }
    }

    /// Monotonic within a boot and never reused: a *slot index* is reused the
    /// instant a mount is released, which would make a paged listing keyed on
    /// position drop or repeat an entry when the set changes between pages.
    /// Exhaustion refuses the mount rather than wrapping. Id 0 means "before
    /// any mount", so a resumption cursor can start there.
    fn alloc_id(&mut self) -> VfsResult<u32> {
        let id = self.next_id;
        if id == u32::MAX {
            return Err(VfsError::NoSpace);
        }
        self.next_id = id + 1;
        Ok(id)
    }

    pub fn mount(&mut self, path: &[u8], fs: &'static dyn FileSystem, flags: u32) -> VfsResult<()> {
        // Canonical, so `/tmp/` and `/tmp` cannot both occupy the table while
        // only one of them is matchable by the per-component walk.
        let canon = canonicalise(path)?;
        let path = canon.as_bytes();

        for mp in self.mounts.iter() {
            if mp.is_active() && mp.path_bytes() == path {
                return Err(VfsError::AlreadyExists);
            }
        }

        let slot_idx = self
            .mounts
            .iter()
            .position(|m| !m.is_active())
            .ok_or(VfsError::NoSpace)?;
        let id = self.alloc_id()?;

        let slot = &mut self.mounts[slot_idx];
        slot.path[..path.len()].copy_from_slice(path);
        slot.path_len = path.len();
        slot.fs = Some(fs);
        slot.flags = flags;
        slot.id = id;
        self.count += 1;

        Ok(())
    }

    pub fn unmount(&mut self, path: &[u8]) -> VfsResult<()> {
        let canon = canonicalise(path)?;
        let path = canon.as_bytes();

        for mp in self.mounts.iter_mut() {
            if mp.is_active() && mp.path_bytes() == path {
                mp.fs = None;
                mp.path_len = 0;
                mp.flags = 0;
                mp.id = 0;
                self.count -= 1;
                return Ok(());
            }
        }
        Err(VfsError::NotFound)
    }

    /// Once per mount point, so a filesystem mounted twice is visited twice.
    pub fn for_each_mount(&self, callback: &mut dyn FnMut(&'static dyn FileSystem)) {
        for mp in self.mounts.iter() {
            if let Some(fs) = mp.fs {
                callback(fs);
            }
        }
    }

    /// Whether `name` is a direct child mount of `parent_path`.
    pub fn has_child_mount(&self, parent_path: &[u8], name: &[u8]) -> bool {
        let parent = trim_trailing_slashes(parent_path);
        self.mounts
            .iter()
            .any(|mp| mp.is_active() && child_component(parent, mp.path_bytes()) == Some(name))
    }

    /// Direct child mounts of `parent_path` in ascending id order, resuming
    /// strictly after `after_id`. Id order rather than slot order is what
    /// makes a paged listing resumable across a concurrent mount or unmount.
    pub fn for_each_child_mount_from(
        &self,
        parent_path: &[u8],
        after_id: u32,
        callback: &mut dyn FnMut(u32, &[u8]) -> bool,
    ) {
        let parent = trim_trailing_slashes(parent_path);
        let mut cursor = after_id;

        loop {
            // Selection scan over the whole table: id order with no
            // allocation and no sorted snapshot.
            let mut next: Option<(u32, &[u8])> = None;
            for mp in self.mounts.iter() {
                if !mp.is_active() || mp.id <= cursor {
                    continue;
                }
                let Some(name) = child_component(parent, mp.path_bytes()) else {
                    continue;
                };
                if next.is_none_or(|(id, _)| mp.id < id) {
                    next = Some((mp.id, name));
                }
            }

            let Some((id, name)) = next else {
                return;
            };
            cursor = id;
            if !callback(id, name) {
                return;
            }
        }
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
pub fn mount_at(path: &[u8]) -> Option<Mounted> {
    let guard = MOUNT_TABLE.read();
    guard
        .mounts
        .iter()
        .find(|mp| mp.is_active() && mp.path_bytes() == path)
        .and_then(|mp| {
            mp.fs.map(|fs| Mounted {
                fs,
                flags: mp.flags,
                id: mp.id,
            })
        })
}
