use crate::vfs::mount::{MAX_MOUNTS, with_mount_table};
use crate::vfs::path::{resolve_parent, resolve_path};
use crate::vfs::traits::{FileType, InodeId, VfsError, VfsResult};
use slopos_abi::fs::{
    FS_TYPE_CHARDEV, FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_SYMLINK, FS_TYPE_UNKNOWN, UserFsEntry,
};
use slopos_ostd::KVec;

pub struct VfsHandle {
    pub inode: InodeId,
    pub fs: &'static dyn crate::vfs::FileSystem,
    /// Granted at open, where the read-only mount and the seal were checked;
    /// a handle without it cannot become a writer later.
    writable: bool,
}

impl VfsHandle {
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read(self.inode, offset, buf)
    }

    pub fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if !self.writable {
            return Err(VfsError::PermissionDenied);
        }
        self.fs.write(self.inode, offset, buf)
    }

    pub fn size(&self) -> VfsResult<u64> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.size)
    }

    pub fn is_directory(&self) -> VfsResult<bool> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.file_type == FileType::Directory)
    }
}

pub struct VfsOpenFlags {
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
    pub writable: bool,
}

impl VfsOpenFlags {
    pub const fn read_only() -> Self {
        Self {
            create: false,
            exclusive: false,
            truncate: false,
            writable: false,
        }
    }

    pub const fn create_only() -> Self {
        Self {
            create: true,
            exclusive: false,
            truncate: false,
            writable: true,
        }
    }
}

pub fn vfs_open(path: &[u8], create: bool) -> VfsResult<VfsHandle> {
    vfs_open_flags(
        path,
        VfsOpenFlags {
            create,
            exclusive: false,
            truncate: false,
            writable: create,
        },
    )
}

pub fn vfs_open_flags(path: &[u8], flags: VfsOpenFlags) -> VfsResult<VfsHandle> {
    match resolve_path(path) {
        Ok(resolved) => {
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let stat = resolved.fs.stat(resolved.inode)?;
            if stat.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }
            // Refused at open, not at the first write: a descriptor obtained
            // before the check is a descriptor that outlives it.
            if flags.writable {
                resolved.check_writable()?;
                if stat.sealed {
                    return Err(VfsError::PermissionDenied);
                }
            }
            if flags.truncate && flags.writable && stat.file_type == FileType::Regular {
                resolved.fs.truncate(resolved.inode, 0)?;
            }
            Ok(VfsHandle {
                inode: resolved.inode,
                fs: resolved.fs,
                writable: flags.writable,
            })
        }
        Err(VfsError::NotFound) if flags.create => {
            let (parent, name) = resolve_parent(path)?;
            parent.check_writable()?;
            let new_inode = parent.fs.create(parent.inode, name, FileType::Regular)?;
            Ok(VfsHandle {
                inode: new_inode,
                fs: parent.fs,
                writable: flags.writable,
            })
        }
        Err(e) => Err(e),
    }
}

pub fn vfs_stat(path: &[u8]) -> VfsResult<(u8, u32)> {
    let resolved = resolve_path(path)?;
    let stat = resolved.fs.stat(resolved.inode)?;

    let kind = match stat.file_type {
        FileType::Directory => FS_TYPE_DIRECTORY,
        FileType::Regular => FS_TYPE_FILE,
        _ => FS_TYPE_UNKNOWN,
    };

    Ok((kind, stat.size as u32))
}

pub fn vfs_mkdir(path: &[u8]) -> VfsResult<()> {
    let (parent, name) = resolve_parent(path)?;
    parent.check_writable()?;
    parent.fs.create(parent.inode, name, FileType::Directory)?;
    Ok(())
}

pub fn vfs_set_mode(path: &[u8], mode: u16) -> VfsResult<()> {
    if path_is_sealed(path) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path(path)?;
    resolved.check_writable()?;
    resolved.fs.set_mode(resolved.inode, mode)
}

/// Seal `path` against every future mutation. One-way and un-clearable.
pub fn vfs_set_sealed(path: &[u8]) -> VfsResult<()> {
    let resolved = resolve_path(path)?;
    resolved.check_writable()?;
    resolved.fs.set_sealed(resolved.inode)
}

/// Whether `path` names a sealed inode.
///
/// Fails **closed**: a resolve or stat that errors for any reason other than
/// the path not existing answers "sealed". This gate is what stops a task
/// replacing `/bin/compositor` and inheriting that path's privilege grant, so
/// an induced `IoError` or an out-of-memory in the block cache must not read
/// as permission. A path that genuinely resolves to nothing is not sealed —
/// the caller's own lookup reports that, and `NotFound` is the one error that
/// carries no ambiguity.
fn path_is_sealed(path: &[u8]) -> bool {
    match resolve_path(path).and_then(|r| r.fs.stat(r.inode)) {
        Ok(stat) => stat.sealed,
        // These four are decided before any inode is consulted — the first two
        // lexically, by `canonicalise` — so a path that trips them cannot be
        // naming a sealed inode. Answering "sealed" would turn `EINVAL` and
        // `ENAMETOOLONG` into `EACCES` for every caller.
        Err(VfsError::InvalidPath)
        | Err(VfsError::NameTooLong)
        | Err(VfsError::NotFound)
        | Err(VfsError::NotDirectory) => false,
        Err(_) => true,
    }
}

pub fn vfs_unlink(path: &[u8]) -> VfsResult<()> {
    if path_is_sealed(path) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent(path)?;
    parent.check_writable()?;
    parent.fs.unlink(parent.inode, name)
}

/// `rmdir(2)`: remove an empty directory. Refuses a mount point outright —
/// removing the directory a filesystem is mounted on would leave the mount
/// table naming a path with nothing behind it.
pub fn vfs_rmdir(path: &[u8]) -> VfsResult<()> {
    if path_is_sealed(path) {
        return Err(VfsError::PermissionDenied);
    }
    let canon = crate::vfs::canon::canonicalise(path)?;
    if crate::vfs::mount::mount_at(canon.as_bytes()).is_some() {
        return Err(VfsError::Busy);
    }
    let (parent, name) = resolve_parent(path)?;
    parent.check_writable()?;
    parent.fs.rmdir(parent.inode, name)
}

/// Create a symlink at `link_path` pointing at `target`.
///
/// The seal is checked on `link_path` as it is for every other mutator: a
/// symlink written over a sealed name would redirect the path a privilege
/// grant is keyed on. The filesystem refuses a duplicate name underneath
/// (`Ext2Fs::create_inode_entry`), so this is defence in depth rather than the
/// only barrier — which is exactly what the seal warrants.
pub fn vfs_symlink(target: &[u8], link_path: &[u8]) -> VfsResult<()> {
    if target.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if path_is_sealed(link_path) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent(link_path)?;
    parent.check_writable()?;
    parent.fs.symlink(parent.inode, name, target).map(|_| ())
}

pub fn vfs_readlink(path: &[u8], buf: &mut [u8]) -> VfsResult<usize> {
    let resolved = resolve_path(path)?;
    let stat = resolved.fs.stat(resolved.inode)?;
    if stat.file_type != FileType::Symlink {
        return Err(VfsError::InvalidArgument);
    }
    resolved.fs.readlink(resolved.inode, buf)
}

pub fn vfs_rename(old_path: &[u8], new_path: &[u8]) -> VfsResult<()> {
    // Both ends: renaming a sealed file moves it out from under the path its
    // privilege is keyed on, and renaming over one replaces it just as a write
    // would.
    if path_is_sealed(old_path) || path_is_sealed(new_path) {
        return Err(VfsError::PermissionDenied);
    }
    let (old_parent, old_name) = resolve_parent(old_path)?;
    let (new_parent, new_name) = resolve_parent(new_path)?;

    if !core::ptr::eq(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    old_parent.check_writable()?;
    new_parent.check_writable()?;

    old_parent
        .fs
        .rename(old_parent.inode, old_name, new_parent.inode, new_name)
}

/// Commits every mount, returning the first error. Snapshots the table and
/// drops its `IrqRwLock` before the first `sync`: ext2's takes a sleeping
/// mutex, which must not be acquired under it.
pub fn vfs_sync_all() -> VfsResult<()> {
    let mut snapshot: [Option<&'static dyn crate::vfs::FileSystem>; MAX_MOUNTS] =
        [None; MAX_MOUNTS];
    let mut n = 0usize;
    with_mount_table(|table| {
        table.for_each_mount(&mut |fs| {
            if n < snapshot.len() {
                snapshot[n] = Some(fs);
                n += 1;
            }
        });
    });

    let mut first_err = None;
    for fs in snapshot.iter().take(n).flatten() {
        if let Err(e) = fs.sync() {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// One-shot listing: the first page only, so a directory larger than `entries`
/// is cut off at the buffer. A caller that must see every entry uses
/// [`vfs_list_from`] and carries its cursor.
pub fn vfs_list(path: &[u8], entries: &mut [UserFsEntry]) -> VfsResult<usize> {
    let mut cursor = ListCursor::start();
    vfs_list_from(path, entries, &mut cursor)
}

/// Where a paged listing resumes. Opaque to userland: the filesystem chooses
/// what its cookie means, and the mount-point pass carries its own index
/// because those entries are synthesised by the VFS rather than read from the
/// filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListCursor {
    /// The filesystem's own resumption point, or [`Self::DIR_DONE`] once the
    /// directory itself is exhausted and only mount entries remain.
    fs_cookie: u64,
    /// How many child mount points have already been listed.
    mounts_done: u32,
}

impl ListCursor {
    const DIR_DONE: u64 = u64::MAX;
    /// Set in the packed form once the filesystem walk is done. A filesystem
    /// cookie is refused rather than truncated if it would collide.
    const MOUNT_PHASE: u64 = 1 << 63;

    pub const fn start() -> Self {
        Self {
            fs_cookie: 0,
            mounts_done: 0,
        }
    }

    pub fn is_end(&self) -> bool {
        self.fs_cookie == Self::DIR_DONE && self.mounts_done == u32::MAX
    }

    /// Pack into the single opaque `u64` the ABI carries.
    pub fn to_abi(self) -> u64 {
        if self.is_end() {
            return slopos_abi::fs::FS_LIST_CURSOR_END;
        }
        if self.fs_cookie == Self::DIR_DONE {
            return Self::MOUNT_PHASE | (self.mounts_done as u64);
        }
        self.fs_cookie
    }

    pub fn from_abi(raw: u64) -> Self {
        if raw == slopos_abi::fs::FS_LIST_CURSOR_END {
            return Self {
                fs_cookie: Self::DIR_DONE,
                mounts_done: u32::MAX,
            };
        }
        if raw & Self::MOUNT_PHASE != 0 {
            return Self {
                fs_cookie: Self::DIR_DONE,
                mounts_done: (raw & 0xFFFF_FFFF) as u32,
            };
        }
        Self {
            fs_cookie: raw,
            mounts_done: 0,
        }
    }
}

/// Fill `entries` from `cursor`, advancing it to where the next call resumes.
///
/// Answers how many entries were written. A buffer that fills mid-directory is
/// neither an error nor a truncation: the cursor names the next entry, which
/// is what makes a directory of any size listable. The listing ends when
/// `cursor.is_end()`.
pub fn vfs_list_from(
    path: &[u8],
    entries: &mut [UserFsEntry],
    cursor: &mut ListCursor,
) -> VfsResult<usize> {
    let resolved = resolve_path(path)?;
    let stat = resolved.fs.stat(resolved.inode)?;

    if stat.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    if entries.is_empty() {
        return Err(VfsError::InvalidArgument);
    }

    let mut count = 0usize;
    let max = entries.len();
    // Sized with the buffer rather than fixed at 64: this holds the inode of
    // every entry written, which the second `stat` pass reads back.
    let mut inodes = KVec::<u64>::zeroed(max).map_err(|_| VfsError::NoSpace)?;

    if cursor.fs_cookie != ListCursor::DIR_DONE {
        resolved.fs.readdir_cookie(
            resolved.inode,
            cursor.fs_cookie,
            &mut |next, name, inode, file_type| {
                // The callback's own bound, not merely the `count < max`
                // return below: correctness must not rest on every filesystem
                // honouring a stop request promptly, because an index past the
                // end panics the kernel in a `forbid(unsafe_code)` crate.
                if count >= max {
                    return false;
                }
                let entry = &mut entries[count];
                *entry = UserFsEntry::new();

                let nlen = name.len().min(entry.name.len() - 1);
                entry.name[..nlen].copy_from_slice(&name[..nlen]);
                entry.name[nlen] = 0;

                entry.type_ = match file_type {
                    FileType::Directory => FS_TYPE_DIRECTORY,
                    FileType::Regular => FS_TYPE_FILE,
                    FileType::Symlink => FS_TYPE_SYMLINK,
                    FileType::CharDevice => FS_TYPE_CHARDEV,
                    _ => FS_TYPE_UNKNOWN,
                };

                inodes[count] = inode;
                count += 1;
                // Advanced past this entry *before* the buffer-full check, so
                // a resumed call does not repeat it.
                cursor.fs_cookie = next;
                count < max
            },
        )?;
        if count < max {
            cursor.fs_cookie = ListCursor::DIR_DONE;
        } else if cursor.fs_cookie >= ListCursor::MOUNT_PHASE {
            // A cookie that cannot round-trip through the ABI would resume the
            // walk somewhere else entirely.
            return Err(VfsError::InvalidArgument);
        }
    }

    for i in 0..count {
        if let Ok(child_stat) = resolved.fs.stat(inodes[i]) {
            entries[i].size = child_stat.size as u32;
        }
    }

    if cursor.fs_cookie != ListCursor::DIR_DONE {
        return Ok(count);
    }

    // Mount points appear as directory entries in the parent listing even when
    // the underlying filesystem has no matching entry (Linux VFS behaviour).
    let skip = cursor.mounts_done;
    let mut seen = 0u32;
    let mut exhausted = true;
    with_mount_table(|mt| {
        mt.for_each_child_mount(path, &mut |child_name| {
            seen += 1;
            if seen <= skip {
                return true;
            }
            if count >= max {
                exhausted = false;
                return false;
            }
            cursor.mounts_done = seen;

            // A mount point always lists as a directory, whatever the entry it
            // shadows was. Only within this page: the shadowed entry may have
            // gone out in an earlier one, in which case the synthesised entry
            // below is a duplicate name — a cheaper defect than a listing that
            // omits a mount, and the same trade the linear cookie walk makes.
            for i in 0..count {
                let elen = entries[i]
                    .name
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(entries[i].name.len());
                if elen == child_name.len() && &entries[i].name[..elen] == child_name {
                    entries[i].type_ = FS_TYPE_DIRECTORY;
                    return true;
                }
            }

            let entry = &mut entries[count];
            *entry = UserFsEntry::new();
            let nlen = child_name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&child_name[..nlen]);
            entry.name[nlen] = 0;
            entry.type_ = FS_TYPE_DIRECTORY;
            entry.size = 0;
            count += 1;
            true
        });
    });

    if exhausted {
        cursor.mounts_done = u32::MAX;
    }

    Ok(count)
}
