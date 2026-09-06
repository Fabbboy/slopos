use slopos_abi::Errno;
use slopos_abi::fs::{ST_RDONLY, UserStatfs};

use slopos_fs::fileio::file_statfs_fd;
use slopos_fs::vfs::{FsStats, MOUNT_RDONLY, vfs_statfs};

use slopos_mm::user_copy::copy_to_user;

use crate::syscall::args::{Fd, UserCStr, UserPtr};
use crate::syscall::common::USER_PATH_MAX;

/// Every field is written: the struct is copied out of this frame, so a field
/// left alone is a field of kernel stack handed to userland.
fn statfs_encode(stats: &FsStats, mount_flags: u32) -> UserStatfs {
    let read_only = mount_flags & MOUNT_RDONLY != 0 || stats.read_only;
    let block_size = u64::from(stats.block_size);
    UserStatfs {
        f_type: stats.magic,
        f_bsize: block_size,
        f_blocks: stats.blocks,
        f_bfree: stats.blocks_free,
        f_bavail: stats.blocks_available,
        f_files: stats.inodes,
        f_ffree: stats.inodes_free,
        // This kernel mints no per-filesystem id, so `f_fsid` stays reserved.
        f_fsid: 0,
        f_namelen: u64::from(stats.max_name_len),
        // No filesystem here addresses fragments smaller than a block.
        f_frsize: block_size,
        f_flags: if read_only { ST_RDONLY } else { 0 },
        _spare: [0; 4],
    }
}

/// Its own frame: the path walk stages a canonical path buffer, on top of the
/// caller's `UserCStr<USER_PATH_MAX>`.
#[inline(never)]
fn statfs_of_path(path: &[u8]) -> Result<UserStatfs, Errno> {
    let (stats, mount_flags) = vfs_statfs(path).map_err(|e| e.to_errno())?;
    Ok(statfs_encode(&stats, mount_flags))
}

define_syscall!(syscall_statfs
    (ctx, path: UserCStr<USER_PATH_MAX>, out: UserPtr<UserStatfs>)
    cap(NoneFd)
    -> Result<(), Errno>
{
    let stats = statfs_of_path(path.as_bytes())?;
    copy_to_user(out.inner(), &stats).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

// A descriptor does not record the mount it was opened through, so `f_flags`
// reports `ST_RDONLY` here only when the filesystem itself refuses writes.
define_syscall!(syscall_fstatfs
    (ctx, fd: Fd, out: UserPtr<UserStatfs>)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let stats = statfs_encode(&file_statfs_fd(pid, fd.raw())?, 0);
    copy_to_user(out.inner(), &stats).map_err(|_| Errno::EFAULT)?;
    Ok(())
});
