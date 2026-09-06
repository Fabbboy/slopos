//! `mount(2)` and `umount2(2)`.
//!
//! Both sit behind `Capability::Mount`, checked by the dispatcher.
//!
//! `mount(2)` cannot conjure a `&'static dyn FileSystem`, so the set of
//! mountable types is closed: a pooled ramfs instance, the devfs singleton, or
//! the one ext2 instance the boot step attached. Anything else is `ENODEV`.

use slopos_abi::Errno;
use slopos_abi::fs::{MNT_DETACH, MOUNT_FSTYPE_MAX, MS_RDONLY};
use slopos_fs::ext2_vfs::{EXT2_VFS_STATIC, ext2_vfs_is_initialized, ext2_vfs_is_read_only};
use slopos_fs::vfs::canon::canonicalise;
use slopos_fs::vfs::init::{vfs_devfs_instance, vfs_ramfs_pool_claim, vfs_ramfs_pool_release};
use slopos_fs::vfs::mount::{MOUNT_RDONLY, mount, mount_at, unmount, with_mount_table};
use slopos_fs::vfs::orphan::{drain_releasable, forget_filesystem, has_open_refs};
use slopos_fs::vfs::traits::{FileSystem, FileType, same_filesystem};
use slopos_fs::vfs::{VfsError, resolve_path};

use crate::syscall::args::UserCStr;
use crate::syscall::common::USER_PATH_MAX;

define_syscall!(syscall_mount
    (ctx,
     source: UserCStr<USER_PATH_MAX>,
     target: UserCStr<USER_PATH_MAX>,
     fstype: UserCStr<MOUNT_FSTYPE_MAX>,
     flags: u32)
    cap(Mount)
    -> Result<(), Errno>
{
    mount_apply(source.as_bytes(), target.as_bytes(), fstype.as_bytes(), flags)
});

define_syscall!(syscall_umount2
    (ctx, path: UserCStr<USER_PATH_MAX>, flags: u32)
    cap(Mount)
    -> Result<(), Errno>
{
    umount_path(path.as_bytes(), flags)
});

fn target_is_directory(path: &[u8]) -> Result<bool, Errno> {
    let resolved = resolve_path(path).map_err(vfs_errno)?;
    let stat = resolved.fs.stat(resolved.inode).map_err(vfs_errno)?;
    Ok(stat.file_type == FileType::Directory)
}

fn vfs_errno(e: VfsError) -> Errno {
    e.to_errno()
}

#[inline(never)]
pub(crate) fn mount_apply(
    source: &[u8],
    target: &[u8],
    fstype: &[u8],
    flags: u32,
) -> Result<(), Errno> {
    let canon = canonicalise(target).map_err(vfs_errno)?;
    let target = canon.as_bytes();

    // The root mount is boot's: every open descriptor and every cached
    // resolution names the filesystem underneath it.
    if target == b"/" {
        return Err(Errno::EBUSY);
    }
    // `EPERM`, not `EBUSY`: covering the path a grant is keyed on would let
    // the caller hand itself that privilege, so the target is permanently
    // refused rather than merely occupied.
    if crate::exec::grants::covers_grant_path(target) {
        return Err(Errno::EPERM);
    }
    // Before the directory check: an occupied path is by construction a valid
    // target, and asking the filesystem underneath yields a worse error.
    if mount_at(target).is_some() {
        return Err(Errno::EBUSY);
    }
    if !target_is_directory(target)? {
        return Err(Errno::ENOTDIR);
    }

    let mount_flags = if flags & MS_RDONLY != 0 {
        MOUNT_RDONLY
    } else {
        0
    };

    match fstype {
        b"ramfs" => {
            // `ENOSPC` as for a full mount table: a fixed kernel table with
            // no room left, which is not `EMFILE`'s per-process ceiling.
            let fs = vfs_ramfs_pool_claim().ok_or(Errno::ENOSPC)?;
            let instance: &'static dyn FileSystem = fs;
            match mount(target, instance, mount_flags) {
                Ok(()) => Ok(()),
                Err(e) => {
                    vfs_ramfs_pool_release(instance, false);
                    Err(vfs_errno(e))
                }
            }
        }
        b"devfs" => mount(target, vfs_devfs_instance(), mount_flags).map_err(vfs_errno),
        b"ext2" => {
            if !ext2_vfs_is_initialized() {
                return Err(Errno::ENODEV);
            }
            // One ext2 instance exists, bound at boot to the `root=` device,
            // so a named `source` asks for a second that cannot exist. Empty
            // means "the attached one".
            if !source.is_empty() {
                return Err(Errno::EBUSY);
            }
            let flags = if ext2_vfs_is_read_only() {
                mount_flags | MOUNT_RDONLY
            } else {
                mount_flags
            };
            mount(target, &EXT2_VFS_STATIC, flags).map_err(vfs_errno)
        }
        _ => Err(Errno::ENODEV),
    }
}

/// How many mount points name `fs`.
///
/// A mount-table scan under its own lock with no filesystem call, so it is
/// safe on any path `umount2` reaches.
fn mounts_of(fs: &'static dyn FileSystem) -> usize {
    let mut count = 0usize;
    with_mount_table(|table| {
        table.for_each_mount(&mut |mounted| {
            if same_filesystem(mounted, fs) {
                count += 1;
            }
        });
    });
    count
}

#[inline(never)]
pub(crate) fn umount_path(path: &[u8], flags: u32) -> Result<(), Errno> {
    let canon = canonicalise(path).map_err(vfs_errno)?;
    let target = canon.as_bytes();

    if target == b"/" {
        return Err(Errno::EBUSY);
    }
    let mounted = mount_at(target).ok_or(Errno::EINVAL)?;

    // Asked of the *mount*, not of the instance: devfs and the ext2 singleton
    // can sit at several paths, and tearing an instance's records down while
    // another mount survives drops that mount's deferred frees and reports
    // `EBUSY` for a descriptor unrelated to the name being removed.
    let last_mount = mounts_of(mounted.fs) <= 1;

    // `MNT_DETACH` is cheap here because every filesystem is a `static`: the
    // descriptor holds a `&'static dyn FileSystem` and stays readable after
    // the name is gone.
    let detach = flags & MNT_DETACH != 0;
    let busy = last_mount && has_open_refs(mounted.fs);
    if busy && !detach {
        return Err(Errno::EBUSY);
    }

    if last_mount {
        let _ = mounted.fs.sync();
        // Before `forget_filesystem`: records left behind keep
        // `releasable_count()` nonzero, which keeps ext2's flusher awake for
        // an obligation nobody will ever run.
        drain_releasable(mounted.fs);
        if !busy {
            forget_filesystem(mounted.fs);
        }
    }

    unmount(target).map_err(vfs_errno)?;
    if last_mount {
        vfs_ramfs_pool_release(mounted.fs, busy);
    }
    Ok(())
}
