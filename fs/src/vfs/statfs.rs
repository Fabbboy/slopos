//! `statfs(2)`'s path entry point.

use crate::vfs::path::resolve_path;
use crate::vfs::traits::{FsStats, VfsResult};

/// The capacity of the filesystem `path` resolves through, together with the
/// flags of the mount the walk ended in.
///
/// The flags describe how the mount was *made*; a filesystem that latched
/// itself read-only afterwards — ext2's `errors=remount-ro` — appears only in
/// [`FsStats::read_only`], which is why the syscall layer folds both into
/// `ST_RDONLY`.
pub fn vfs_statfs(path: &[u8]) -> VfsResult<(FsStats, u32)> {
    let resolved = resolve_path(path)?;
    let stats = resolved.fs.statfs()?;
    Ok((stats, resolved.mount_flags))
}
