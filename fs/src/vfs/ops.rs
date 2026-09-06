use crate::vfs::mount::{MAX_MOUNTS, MountTable, with_mount_table};
use crate::vfs::path::{ResolvedPath, resolve_parent, resolve_path};
use crate::vfs::traits::{FileType, InodeId, VfsError, VfsResult, same_filesystem};
use slopos_abi::fs::{
    FS_TYPE_BLOCKDEV, FS_TYPE_CHARDEV, FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_SYMLINK,
    FS_TYPE_UNKNOWN, UserFsEntry,
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

/// One creation-time name limit whatever the root filesystem is: a name a
/// disk root would accept but `UserFsEntry.name`'s 64 bytes cannot hold would
/// list truncated and then fail to open.
fn check_name_len(name: &[u8]) -> VfsResult<()> {
    if name.len() > crate::MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
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
                // `forget_inode`, not `detach_inode`: a flush would write back
                // the bytes the truncate discards, and a set left keyed would
                // put pre-truncate pages back over the zeroed region.
                crate::filemap::forget_inode(resolved.fs, resolved.inode);
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
            check_name_len(name)?;
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
        FileType::CharDevice => FS_TYPE_CHARDEV,
        FileType::BlockDevice => FS_TYPE_BLOCKDEV,
        _ => FS_TYPE_UNKNOWN,
    };

    Ok((kind, stat.size as u32))
}

pub fn vfs_mkdir(path: &[u8]) -> VfsResult<()> {
    let (parent, name) = resolve_parent(path)?;
    check_name_len(name)?;
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

/// `unlink(2)`: remove a name.
///
/// The inode's contents outlive the name whenever a descriptor still holds
/// them — POSIX's rule, and what stops an `unlink` handing a live reader's
/// blocks to the next allocation. Which of the two removals runs is decided by
/// the open-reference count, under the lock that count is taken under, so an
/// inode opened concurrently is never freed out from under the opener.
///
/// The cheap path is the common one: nothing holds the inode open, and the
/// filesystem frees it with the name exactly as before.
pub fn vfs_unlink(path: &[u8]) -> VfsResult<()> {
    use crate::vfs::orphan::{DetachPlan, RemovalOutcome, begin_removal, end_removal};

    if path_is_sealed(path) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent(path)?;
    parent.check_writable()?;

    // A name that resolves to nothing cannot be holding an inode open, and a
    // filesystem that reports its own `ENOENT` gives a better error than a
    // lookup here would.
    let Ok(inode) = parent.fs.lookup(parent.inode, name) else {
        return parent.fs.unlink(parent.inode, name);
    };

    // Before the removal: the flush is only safe while the inode's blocks are
    // still its own, and the forget must land before the inode number can be
    // reallocated.
    crate::filemap::detach_inode(parent.fs, inode);

    if begin_removal(parent.fs, inode) == DetachPlan::FreeNow {
        let result = parent.fs.unlink(parent.inode, name);
        let _ = end_removal(parent.fs, inode, RemovalOutcome::Nothing);
        return result;
    }

    let result = parent.fs.detach(parent.inode, name);
    let outcome = match result {
        Ok(Some(_)) => RemovalOutcome::Deferred,
        _ => RemovalOutcome::Nothing,
    };
    // A close that landed inside the scope above is the one case where the
    // free becomes runnable with nobody left to notice.
    if end_removal(parent.fs, inode, outcome) {
        crate::vfs::orphan::drain_or_wake(parent.fs);
    }
    result.map(|_| ())
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
    // Only a regular file can carry a page set while `mmap` refuses every
    // other type, and that is not a rule to leave load-bearing.
    if let Ok(inode) = parent.fs.lookup(parent.inode, name) {
        crate::filemap::detach_inode(parent.fs, inode);
    }
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
    check_name_len(name)?;
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
    use crate::vfs::orphan::{DetachPlan, RemovalOutcome, begin_removal, end_removal};

    // Both ends: renaming a sealed file moves it out from under the path its
    // privilege is keyed on, and renaming over one replaces it just as a write
    // would.
    if path_is_sealed(old_path) || path_is_sealed(new_path) {
        return Err(VfsError::PermissionDenied);
    }
    let (old_parent, old_name) = resolve_parent(old_path)?;
    let (new_parent, new_name) = resolve_parent(new_path)?;
    check_name_len(new_name)?;

    if !same_filesystem(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    old_parent.check_writable()?;
    new_parent.check_writable()?;

    // Renaming *over* an open file is the same hazard as unlinking one: the
    // displaced name was that inode's last, and freeing it hands a live
    // reader's blocks away. A destination that names nothing, or nothing open,
    // takes the plain path.
    let displaced = new_parent.fs.lookup(new_parent.inode, new_name).ok();
    let Some(displaced) = displaced else {
        return old_parent
            .fs
            .rename(old_parent.inode, old_name, new_parent.inode, new_name);
    };

    crate::filemap::detach_inode(new_parent.fs, displaced);

    if begin_removal(new_parent.fs, displaced) == DetachPlan::FreeNow {
        let result = old_parent
            .fs
            .rename(old_parent.inode, old_name, new_parent.inode, new_name);
        let _ = end_removal(new_parent.fs, displaced, RemovalOutcome::Nothing);
        return result;
    }

    let result =
        old_parent
            .fs
            .rename_detaching(old_parent.inode, old_name, new_parent.inode, new_name);
    let outcome = match result {
        Ok(Some(_)) => RemovalOutcome::Deferred,
        _ => RemovalOutcome::Nothing,
    };
    if end_removal(new_parent.fs, displaced, outcome) {
        crate::vfs::orphan::drain_or_wake(new_parent.fs);
    }
    result.map(|_| ())
}

/// Commits every mount, returning the first error. Snapshots the table and
/// drops its `IrqRwLock` before the first `sync`: ext2's takes a sleeping
/// mutex, which must not be acquired under it.
pub fn vfs_sync_all() -> VfsResult<()> {
    // Before the mounts: page-set writeback reaches a filesystem through
    // `write`, so it must land before that filesystem's `sync`.
    crate::filemap::flush_all();
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
/// what its cookie means, and the mount-point pass carries the identity of the
/// last mount it emitted, because those entries are synthesised by the VFS
/// rather than read from the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListCursor {
    /// The filesystem's own resumption point, or [`Self::DIR_DONE`] once the
    /// directory itself is exhausted and only mount entries remain.
    fs_cookie: u64,
    /// The last child mount emitted, 0 before the first. Keyed on the mount's
    /// identity, not its table position: a released slot is reused at once, so
    /// an ordinal would drop or repeat entries between pages.
    last_mount_id: u32,
    done: bool,
}

impl ListCursor {
    const DIR_DONE: u64 = u64::MAX;
    /// Set in the packed form once the filesystem walk is done. A filesystem
    /// cookie is refused rather than truncated if it would collide.
    const MOUNT_PHASE: u64 = 1 << 63;

    pub const fn start() -> Self {
        Self {
            fs_cookie: 0,
            last_mount_id: 0,
            done: false,
        }
    }

    pub fn is_end(&self) -> bool {
        self.done
    }

    /// Pack into the single opaque `u64` the ABI carries. A mount id is 32
    /// bits wide, so `MOUNT_PHASE | id` can never collide with
    /// [`slopos_abi::fs::FS_LIST_CURSOR_END`]'s all-ones.
    pub fn to_abi(self) -> u64 {
        if self.done {
            return slopos_abi::fs::FS_LIST_CURSOR_END;
        }
        if self.fs_cookie == Self::DIR_DONE {
            return Self::MOUNT_PHASE | (self.last_mount_id as u64);
        }
        self.fs_cookie
    }

    pub fn from_abi(raw: u64) -> Self {
        if raw == slopos_abi::fs::FS_LIST_CURSOR_END {
            return Self {
                fs_cookie: Self::DIR_DONE,
                last_mount_id: 0,
                done: true,
            };
        }
        if raw & Self::MOUNT_PHASE != 0 {
            return Self {
                fs_cookie: Self::DIR_DONE,
                last_mount_id: (raw & 0xFFFF_FFFF) as u32,
                done: false,
            };
        }
        Self {
            fs_cookie: raw,
            last_mount_id: 0,
            done: false,
        }
    }
}

/// Whether `name`, as a listing page of `dir` stored it, is shadowed by a
/// direct child mount. `truncated` clips the mount's own name the way the ABI
/// entry clipped `name`, or both passes would emit that name.
fn mount_shadows(mt: &MountTable, dir: &[u8], name: &[u8], truncated: bool) -> bool {
    if mt.has_child_mount(dir, name) {
        return true;
    }
    if !truncated {
        return false;
    }
    let mut hit = false;
    mt.for_each_child_mount_from(dir, 0, &mut |_, child| {
        if child.len() > name.len() && &child[..name.len()] == name {
            hit = true;
        }
        !hit
    });
    hit
}

/// Drop from `page` every name a child mount of `dir` shadows, compacting the
/// survivors to the front and answering how many remain. The mount pass is
/// then the single authority for those names across every page.
fn drop_shadowed_names(mt: &MountTable, dir: &[u8], page: &mut [UserFsEntry]) -> usize {
    let mut kept = 0usize;
    for i in 0..page.len() {
        let cap = page[i].name.len();
        let elen = page[i].name.iter().position(|&b| b == 0).unwrap_or(cap);
        if mount_shadows(mt, dir, &page[i].name[..elen], elen == cap - 1) {
            continue;
        }
        if kept != i {
            let entry = page[i];
            page[kept] = entry;
        }
        kept += 1;
    }
    kept
}

/// One filesystem page of a listing.
///
/// The mount table is taken only *after* the filesystem calls have returned:
/// `MOUNT_TABLE` is an `IrqRwLock` at `LOCK_LEVEL_REGISTRY` — IRQs and
/// preemption off — and ext2's own lock is a sleeping mutex, so the table must
/// not be held across block I/O. Never inlined so its frame is not charged to
/// [`vfs_list_from`]'s.
#[inline(never)]
fn list_fs_page(
    resolved: &ResolvedPath,
    dir: &[u8],
    entries: &mut [UserFsEntry],
    inodes: &mut KVec<u64>,
    cursor: &mut ListCursor,
) -> VfsResult<usize> {
    let max = entries.len();
    let mut filled = 0usize;

    resolved.fs.readdir_cookie(
        resolved.inode,
        cursor.fs_cookie,
        &mut |next, name, inode, file_type| {
            // The callback's own bound, not merely the `filled < max`
            // return below: correctness must not rest on every filesystem
            // honouring a stop request promptly, because an index past the
            // end panics the kernel in a `forbid(unsafe_code)` crate.
            if filled >= max {
                return false;
            }
            let entry = &mut entries[filled];
            *entry = UserFsEntry::new();

            let nlen = name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&name[..nlen]);
            entry.name[nlen] = 0;

            entry.type_ = match file_type {
                FileType::Directory => FS_TYPE_DIRECTORY,
                FileType::Regular => FS_TYPE_FILE,
                FileType::Symlink => FS_TYPE_SYMLINK,
                FileType::CharDevice => FS_TYPE_CHARDEV,
                FileType::BlockDevice => FS_TYPE_BLOCKDEV,
                _ => FS_TYPE_UNKNOWN,
            };

            inodes[filled] = inode;
            filled += 1;
            // Advanced past this entry *before* the buffer-full check, so
            // a resumed call does not repeat it.
            cursor.fs_cookie = next;
            filled < max
        },
    )?;

    if filled < max {
        cursor.fs_cookie = ListCursor::DIR_DONE;
    } else if cursor.fs_cookie >= ListCursor::MOUNT_PHASE {
        // A cookie that cannot round-trip through the ABI would resume the
        // walk somewhere else entirely.
        return Err(VfsError::InvalidArgument);
    }

    for i in 0..filled {
        if let Ok(child_stat) = resolved.fs.stat(inodes[i]) {
            entries[i].size = child_stat.size as u32;
        }
    }

    Ok(with_mount_table(|mt| {
        drop_shadowed_names(mt, dir, &mut entries[..filled])
    }))
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

    // The mount table is keyed on canonical paths, so a listing of `//tmp`
    // must ask about `/tmp` or it sees none of that directory's child mounts.
    let canon = crate::vfs::canon::canonicalise(path)?;
    let dir = canon.as_bytes();

    let max = entries.len();
    // Sized with the buffer rather than fixed at 64: this holds the inode of
    // every entry written, which the second `stat` pass reads back.
    let mut inodes = KVec::<u64>::zeroed(max).map_err(|_| VfsError::NoSpace)?;

    let mut count = 0usize;
    while cursor.fs_cookie != ListCursor::DIR_DONE {
        count = list_fs_page(&resolved, dir, entries, &mut inodes, cursor)?;
        // An all-shadowed page must not go back empty: an empty page is how a
        // finished listing looks to a caller. Bounded, since a whole listing
        // can drop at most `MAX_MOUNTS` names.
        if count > 0 {
            break;
        }
    }

    if cursor.fs_cookie != ListCursor::DIR_DONE {
        return Ok(count);
    }

    // Mount points appear as directory entries in the parent listing even when
    // the underlying filesystem has no matching entry (Linux VFS behaviour).
    let mut exhausted = true;
    with_mount_table(|mt| {
        mt.for_each_child_mount_from(dir, cursor.last_mount_id, &mut |id, child_name| {
            if count >= max {
                exhausted = false;
                return false;
            }
            let entry = &mut entries[count];
            *entry = UserFsEntry::new();
            let nlen = child_name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&child_name[..nlen]);
            entry.name[nlen] = 0;
            // A mount point always lists as a directory, whatever the entry it
            // shadows was.
            entry.type_ = FS_TYPE_DIRECTORY;
            entry.size = 0;
            count += 1;
            cursor.last_mount_id = id;
            true
        });
    });

    if exhausted {
        cursor.done = true;
    }

    Ok(count)
}
