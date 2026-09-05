use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

use crate::blockdev::BlockDevice;
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode, Ext2Superblock, ReadOnlyReason};
use crate::verity::{FsExtent, VerityError, VerityStatus};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult, orphan};
use slopos_ostd::KBox;
use slopos_ostd::klog_info;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, Mutex};

const EXT2_ROOT_INODE: u32 = 2;

struct CachedExt2 {
    /// Sole writable handle to the backing device, held for the kernel's
    /// lifetime so no second writer can be acquired.
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    /// Sized to `block_size` at mount.
    cache: BlockCache,
    /// Free-count drift from a mutating op. Lives here — not only on the
    /// per-call `Ext2Fs` handle — so a later sync sees earlier ops' dirtiness.
    superblock_dirty: bool,
    /// Every handle built over this mount refuses mutation.
    ///
    /// Not derivable from the superblock on the per-call handle: two of the
    /// three reasons are invisible there. `NotCleanlyUnmounted` is decided
    /// against the state as it came off the disk, which the mount stamp then
    /// overwrites, and `ErrorsRemountRo` is a runtime verdict.
    read_only: bool,
}

/// How the device came up at mount, for the boot log and the mounter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext2MountInfo {
    pub verity: VerityStatus,
    pub read_only: bool,
    /// Why writes are refused, when they are.
    pub read_only_reason: Option<ReadOnlyReason>,
    /// Inodes the previous boot left unlinked-but-open, reclaimed at this
    /// mount.
    pub orphans_drained: u32,
    /// `e2fsck`'s own mount-count or check-interval rule says the image is due
    /// a check. Reported, never acted on: this kernel runs no fsck.
    pub check_overdue: bool,
}

/// A *sleeping* mutex: ext2 block-device I/O waits are scheduler-backed, so
/// the holder may legitimately deschedule mid-operation.
static CACHED_EXT2: Mutex<Option<CachedExt2>> =
    Mutex::new(None, lock_class!("CACHED_EXT2", LOCK_LEVEL_RESOURCE));
static EXT2_VFS_INIT: InitFlag = InitFlag::new();
/// Every `Ext2Fs` built over the mounted device refuses mutation. Outside the
/// lock so a query never waits on in-flight block I/O.
static EXT2_READ_ONLY: AtomicBool = AtomicBool::new(false);
/// Set when an operation flipped the mount read-only, as distinct from a mount
/// that came up read-only. Outside the lock, for the same reason
/// [`EXT2_READ_ONLY`] is.
static REMOUNT_RO_PENDING: AtomicBool = AtomicBool::new(false);
/// One-shot, so `errors=remount-ro` logs its cause once rather than on every
/// subsequent operation.
static REMOUNT_RO_REPORTED: InitFlag = InitFlag::new();

/// Best-effort dirty-block count: the flusher's wait predicate reads only this
/// and the stop flag, so it takes no lock.
static DIRTY_PENDING: AtomicUsize = AtomicUsize::new(0);

static FLUSH_STOP: KernelIoStop = KernelIoStop::new(
    "ext2-flush",
    lock_class!("EXT2_FLUSH_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static FLUSH_THREAD_STARTED: InitFlag = InitFlag::new();

/// Periodic writeback cadence — analog of Linux `dirty_writeback_centisecs`.
const FLUSH_INTERVAL_MS: u64 = 5_000;
/// Eager-wake threshold — analog of `dirty_background_ratio`. Past this many
/// dirty blocks a mutating op kicks the flusher instead of awaiting the tick.
const FLUSH_EAGER_THRESHOLD: usize = 48;
/// First retry delay after a failed sync (exponential backoff floor).
const FLUSH_BACKOFF_MIN_MS: u64 = 50;
/// Backoff ceiling — a persistently failing device is retried no more often
/// than the periodic flush cadence.
const FLUSH_BACKOFF_MAX_MS: u64 = FLUSH_INTERVAL_MS;

pub struct StaticExt2Vfs;

impl StaticExt2Vfs {
    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        if !EXT2_VFS_INIT.is_set() {
            return Err(VfsError::IoError);
        }
        let mut guard = CACHED_EXT2.lock().map_err(|_| VfsError::Interrupted)?;
        let cached = guard.as_mut().ok_or(VfsError::IoError)?;
        let result = with_cached_fs(cached, f);
        note_dirty(cached.cache.dirty_count());
        drop(guard);
        // Off-lock: the log line is the point of `errors=remount-ro` and must
        // not be emitted while every path walk on this mount is queued behind
        // the lock it would hold.
        report_remount_ro_if_pending();
        result
    }
}

/// Run one ext2 operation over `cached`, publishing everything it moved.
///
/// The `Ext2Fs` handle is per-call and the mount state is not, so this is the
/// one place that copies between them. Three things travel back: the
/// superblock (its free counts and the fields `e2fsck` reads), the dirty flag,
/// and the corruption verdict — which is what latches the mount read-only.
fn with_cached_fs<R>(
    cached: &mut CachedExt2,
    f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>,
) -> VfsResult<R> {
    let (superblock, block_size, inode_size) =
        (cached.superblock, cached.block_size, cached.inode_size);
    let mut fs = Ext2Fs::new(
        &*cached.device,
        &mut cached.cache,
        superblock,
        block_size,
        inode_size,
    )
    .map_err(ext2_error_to_vfs)?;
    fs.set_superblock_dirty(cached.superblock_dirty);
    if cached.read_only {
        fs.force_read_only();
    }
    // Classified here rather than only inside the mutating entry points,
    // because a *read* that finds a group descriptor pointing outside the
    // volume is the same evidence of damage as a write that does, and a mount
    // that keeps writing after one is what `errors=remount-ro` exists to stop.
    let raw = f(&mut fs);
    let result = fs.note_result(raw).map_err(ext2_error_to_vfs);
    // Every mutating `Ext2Fs` entry point rolls its own dirtied blocks and
    // free counts back on failure, so the post-state is the committed one
    // either way and is published unconditionally. Reading it back only on
    // success would strand a rollback the guard had already performed.
    let new_superblock = fs.superblock();
    let new_superblock_dirty = fs.superblock_dirty();
    let corrupted = fs.corruption_seen();
    drop(fs);

    // Deliberately no per-op flush: dirty blocks stay in the persistent cache
    // until eviction, the background flusher, `sync`, or shutdown.
    cached.superblock = new_superblock;
    cached.superblock_dirty = new_superblock_dirty;
    if corrupted && !cached.read_only {
        // `errors=remount-ro`. Continuing to write into a filesystem already
        // known to be damaged turns a repairable image into a lost one, and
        // the damage is on the disk rather than in this operation, so it is
        // the *mount* that must stop writing rather than this call.
        cached.read_only = true;
        EXT2_READ_ONLY.store(true, Ordering::Release);
        REMOUNT_RO_PENDING.store(true, Ordering::Release);
    }
    result
}

/// The one-shot `errors=remount-ro` log line, emitted off the mount lock.
fn report_remount_ro_if_pending() {
    if !REMOUNT_RO_PENDING.load(Ordering::Acquire) || !REMOUNT_RO_REPORTED.init_once() {
        return;
    }
    klog_info!("ext2: filesystem error — remounting read-only. Repair with e2fsck on the host.");
}

/// Never holds the FS lock.
fn note_dirty(dirty: usize) {
    DIRTY_PENDING.store(dirty, Ordering::Relaxed);
    if dirty >= FLUSH_EAGER_THRESHOLD {
        FLUSH_STOP.wake_one_for_work();
    }
}

/// Wake the flusher to complete a deferred inode free.
///
/// The last close of an unlinked file cannot do the free itself: it runs from
/// a `Drop` that the task-exit path reaches under a preempt guard, and the
/// free takes a sleeping mutex and parks on block I/O. So the close only marks
/// the record releasable and pokes the thread that may block.
pub(crate) fn ext2_vfs_wake_for_detached() {
    if EXT2_VFS_INIT.is_set() {
        FLUSH_STOP.wake_one_for_work();
    }
}

trait Ext2VfsBackend {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R>;
}

/// Commit one inode. Takes the same lock every other ext2 operation does, but
/// writes only that inode's blocks — so a descriptor-granular `fsync` no
/// longer drags every other file's dirty state to the device with it.
fn ext2_vfs_sync_inode(inode: InodeId, data_only: bool) -> VfsResult<()> {
    if !EXT2_VFS_INIT.is_set() {
        return Ok(());
    }
    let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
    let mut guard = CACHED_EXT2.lock().map_err(|_| VfsError::Interrupted)?;
    let Some(cached) = guard.as_mut() else {
        return Ok(());
    };
    let result = with_cached_fs(cached, |fs| fs.sync_inode(ino, data_only));
    // Not a `CLEAN_THROUGH` publication: this committed one inode, so a later
    // whole-filesystem `sync` still owes the device everything else.
    DIRTY_PENDING.store(cached.cache.dirty_count(), Ordering::Relaxed);
    drop(guard);
    report_remount_ro_if_pending();
    result
}

impl Ext2VfsBackend for StaticExt2Vfs {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        self.with_fs(f)
    }
}

impl<T: Ext2VfsBackend + Send + Sync> FileSystem for T {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root_inode(&self) -> InodeId {
        EXT2_ROOT_INODE as InodeId
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let parent_inode = fs.read_inode(parent as u32)?;
            if !parent_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            let mut found: Option<u32> = None;
            fs.for_each_dir_entry(parent as u32, |entry| {
                if entry.name == name {
                    found = Some(entry.inode.raw());
                    false
                } else {
                    true
                }
            })?;
            found.map(|i| i as InodeId).ok_or(Ext2Error::PathNotFound)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            Ok(FileStat {
                inode,
                file_type: inode_to_file_type(&ext2_inode),
                size: ext2_inode.size as u64,
                mode: ext2_inode.mode,
                nlink: ext2_inode.links_count as u32,
                uid: ext2_inode.uid as u32,
                gid: ext2_inode.gid as u32,
                atime: ext2_inode.atime as u64,
                mtime: ext2_inode.mtime as u64,
                ctime: ext2_inode.ctime as u64,
                dev_major: 0,
                dev_minor: 0,
                // `EXT2_IMMUTABLE_FL` is the carrier, so the seal survives a
                // reboot and reads as one to `lsattr` and `e2fsck`.
                sealed: ext2_inode.is_immutable(),
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file(inode as u32, offset, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.write_file(inode as u32, offset, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                _ => return Err(Ext2Error::InvalidInode),
            };
            Ok(inode as InodeId)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_ext2(|fs| fs.unlink_entry(parent as u32, name))
    }

    fn detach(&self, parent: InodeId, name: &[u8]) -> VfsResult<Option<InodeId>> {
        self.with_ext2(|fs| fs.detach_entry(parent as u32, name))
            .map(|o| o.map(|ino| ino as InodeId))
    }

    fn release_detached(&self, inode: InodeId) -> VfsResult<()> {
        let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
        self.with_ext2(|fs| fs.release_orphan(ino))
    }

    /// The flusher kthread is what drains this filesystem's deferred frees.
    fn wake_for_detached(&self) -> bool {
        ext2_vfs_wake_for_detached();
        true
    }

    fn rmdir(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_ext2(|fs| fs.remove_directory(parent as u32, name))
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let mut count = 0usize;
        self.readdir_cookie(inode, offset as u64, &mut |_, name, ino, ft| {
            count += 1;
            callback(name, ino, ft)
        })?;
        Ok(count)
    }

    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            if !ext2_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            fs.for_each_dir_entry_from(inode as u32, cookie, |next, entry| {
                let ft = ext2_file_type_to_vfs(entry.file_type);
                callback(next, entry.name, entry.inode.raw() as InodeId, ft)
            })
        })
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        self.with_ext2(|fs| fs.truncate_file(inode as u32, size))
    }

    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        self.with_ext2(|fs| {
            fs.rename_entry(old_parent as u32, old_name, new_parent as u32, new_name)
        })
    }

    fn rename_detaching(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<Option<InodeId>> {
        self.with_ext2(|fs| {
            fs.rename_entry_with(
                old_parent as u32,
                old_name,
                new_parent as u32,
                new_name,
                crate::ext2::LastLink::Orphan,
            )
        })
        .map(|o| o.map(|ino| ino as InodeId))
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_symlink(inode as u32, buf))
    }

    fn symlink(&self, parent: InodeId, name: &[u8], target: &[u8]) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            fs.create_symlink(parent as u32, name, target)
                .map(|i| i as InodeId)
        })
    }

    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_mode(inode as u32, mode))
    }

    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_sealed(inode as u32))
    }

    fn sync(&self) -> VfsResult<()> {
        ext2_vfs_sync()
    }

    fn sync_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()> {
        ext2_vfs_sync_inode(inode, data_only)
    }
}

pub static EXT2_VFS_STATIC: StaticExt2Vfs = StaticExt2Vfs;

/// Mount the ext2 image on `device`. A verity trailer, when present, makes
/// the mount read-only; a trailer that is present but unusable refuses the
/// mount, so an image claiming attestation is never read unverified.
pub fn ext2_vfs_init_with_device(
    device: KBox<dyn BlockDevice + Send + Sync>,
) -> VfsResult<Ext2MountInfo> {
    // A second call must error rather than silently drop the caller's
    // capability token, which would release the exclusive write claim.
    if !EXT2_VFS_INIT.init_once() {
        return Err(VfsError::AlreadyExists);
    }
    match mount_device(device) {
        Ok(info) => Ok(info),
        Err(e) => {
            EXT2_VFS_INIT.reset();
            Err(e)
        }
    }
}

fn mount_device(device: KBox<dyn BlockDevice + Send + Sync>) -> VfsResult<Ext2MountInfo> {
    // The superblock is read off the raw device: a trailer can only be
    // recognised relative to the extent the filesystem claims, and the
    // sub-block read is one verity would not check anyway.
    let (superblock, block_size, inode_size) =
        Ext2Fs::mount_params(&*device).map_err(ext2_error_to_vfs)?;
    let extent = FsExtent {
        block_size,
        blocks: superblock.blocks_count as u64,
    };
    let (device, verity) = crate::verity::build_verified(device, extent).map_err(|e| {
        klog_info!("verity: refusing to mount — {:?}", e);
        verity_error_to_vfs(e)
    })?;
    log_verity_status(verity);

    // Asked against the superblock as it came off the disk: `install_cached`
    // stamps `EXT2_ERROR_FS` into it, and asking afterwards would read this
    // mount's own stamp as the previous mount's crash.
    let read_only_reason = Ext2Fs::mount_read_only_reason(&superblock, &*device);
    let read_only = read_only_reason.is_some();
    log_read_only_reason(read_only_reason);
    EXT2_READ_ONLY.store(read_only, Ordering::Release);
    install_cached(device, superblock, block_size, inode_size, read_only)?;

    let (orphans_drained, check_overdue) = post_mount_recovery();

    if !read_only {
        start_flusher();
    }
    Ok(Ext2MountInfo {
        verity,
        read_only,
        read_only_reason,
        orphans_drained,
        check_overdue,
    })
}

/// Reclaim what the previous boot left unlinked-but-open, and ask whether the
/// image is due a check. Both need the mount published, so neither can happen
/// inside [`install_cached`].
#[inline(never)]
fn post_mount_recovery() -> (u32, bool) {
    let Ok(mut guard) = CACHED_EXT2.lock() else {
        return (0, false);
    };
    let Some(cached) = guard.as_mut() else {
        return (0, false);
    };
    let drained = with_cached_fs(cached, |fs| {
        let drained = fs.drain_orphans()?;
        let overdue = fs
            .read_bookkeeping()?
            .check_overdue(slopos_kernel_services::clock::realtime_unix_secs());
        Ok((drained, overdue))
    });
    drop(guard);
    match drained {
        Ok((n, overdue)) => (n, overdue),
        Err(e) => {
            klog_info!("ext2: orphan drain failed: {:?}", e);
            (0, false)
        }
    }
}

#[inline(never)]
fn log_read_only_reason(reason: Option<ReadOnlyReason>) {
    let Some(reason) = reason else {
        return;
    };
    match reason {
        ReadOnlyReason::DeviceWriteProtected => {
            klog_info!("ext2: mounting read-only — the device is verity-attested")
        }
        ReadOnlyReason::UnsupportedFeature => klog_info!(
            "ext2: mounting read-only — the image declares a feature this kernel does not write"
        ),
        // Loud on purpose. The image is safe to read and unsafe to write, and
        // the repair tool is on the host: there is no in-kernel fsck and a
        // silent read-only root is the failure mode this line exists to
        // prevent someone debugging for an hour.
        ReadOnlyReason::NotCleanlyUnmounted => klog_info!(
            "ext2: MOUNTING READ-ONLY — the image was never marked clean, so the last \
             boot crashed or is still running. Repair it on the host with \
             `e2fsck -fy <image>`; until then every write returns EROFS."
        ),
        ReadOnlyReason::ErrorsRemountRo => {
            klog_info!("ext2: mounting read-only — a previous error latched the mount")
        }
    }
}

#[inline(never)]
fn log_verity_status(verity: VerityStatus) {
    match verity {
        VerityStatus::Absent => klog_info!("verity: no trailer — image mounts unverified"),
        VerityStatus::Verified { blocks, block_size } => klog_info!(
            "verity: enabled — {} blocks of {} bytes, device write-protected",
            blocks,
            block_size,
        ),
    }
}

/// Build the cache, publish `CACHED_EXT2` and stamp the not-clean bit. Its
/// own frame so the cache temporaries do not share one with the verity parse.
#[inline(never)]
fn install_cached(
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    read_only: bool,
) -> VfsResult<()> {
    let cache = BlockCache::new(block_size).map_err(ext2_error_to_vfs)?;
    let mut guard = CACHED_EXT2.lock().map_err(|_| VfsError::Interrupted)?;
    *guard = Some(CachedExt2 {
        device,
        superblock,
        block_size,
        inode_size: if inode_size == 0 { 128 } else { inode_size },
        cache,
        superblock_dirty: false,
        read_only,
    });
    if let Some(cached) = guard.as_mut() {
        stamp_not_clean(cached);
    }
    Ok(())
}

/// The not-clean bit is what tells a later fsck it must run; without it a
/// crash or an unflushed reboot leaves an image that still claims to be
/// clean. A no-op on a read-only handle, so a write-protected device is never
/// touched.
#[inline(never)]
fn stamp_not_clean(cached: &mut CachedExt2) {
    if cached.read_only {
        return;
    }
    let (sb, bs, is) = (cached.superblock, cached.block_size, cached.inode_size);
    let Ok(mut fs) = Ext2Fs::new(&*cached.device, &mut cached.cache, sb, bs, is) else {
        return;
    };
    if fs.mark_dirty_on_disk().is_ok() {
        cached.superblock = fs.superblock();
    }
}

/// Whether the mounted ext2 filesystem refuses every mutation. `false` when
/// nothing is mounted.
pub fn ext2_vfs_is_read_only() -> bool {
    EXT2_VFS_INIT.is_set() && EXT2_READ_ONLY.load(Ordering::Acquire)
}

fn verity_error_to_vfs(e: VerityError) -> VfsError {
    match e {
        VerityError::UnsupportedTrailer => VfsError::NotSupported,
        VerityError::CorruptTrailer
        | VerityError::Geometry
        | VerityError::Device
        | VerityError::OutOfMemory => VfsError::IoError,
    }
}

/// Takes the FS lock, so the caller must hold none. A no-op if no ext2
/// filesystem is mounted.
///
/// A `sync(2)` storm costs the device one pass rather than one per caller:
/// the second caller in finds nothing dirty and nothing unbarriered, and
/// returns without touching it. What is *not* bounded is the wait — the first
/// caller holds a sleeping mutex across every path walk and `exec` on the
/// mount for the length of its pass. Narrowing that needs a lock finer than
/// one per mount, which is a page-cache change rather than a sync change.
pub fn ext2_vfs_sync() -> VfsResult<()> {
    if !EXT2_VFS_INIT.is_set() {
        return Ok(());
    }
    let Ok(mut guard) = CACHED_EXT2.lock() else {
        return Err(VfsError::Interrupted);
    };
    let Some(cached) = guard.as_mut() else {
        return Ok(());
    };
    // Skip the device barriers rather than issue no-op flushes every tick.
    // Read state, not a completion epoch: an op that *failed* leaves its
    // dirtied blocks cached (see the TODO in `with_fs`), so "a sync already
    // ran" is not evidence that there is nothing left to write.
    if cached.cache.dirty_count() == 0
        && cached.cache.unbarriered_writes() == 0
        && !cached.superblock_dirty
    {
        DIRTY_PENDING.store(0, Ordering::Relaxed);
        return Ok(());
    }
    let result = with_cached_fs(cached, |fs| fs.sync());
    DIRTY_PENDING.store(cached.cache.dirty_count(), Ordering::Relaxed);
    drop(guard);
    report_remount_ro_if_pending();
    result
}

/// Must be called with interrupts still enabled — the virtio-blk completion
/// path needs them. Best-effort.
pub fn ext2_vfs_shutdown_sync() {
    FLUSH_STOP.request();
    // An orphan whose last descriptor closed during shutdown teardown is one
    // this boot can still free, and a free left to the next mount's drain is
    // one that costs a mount-time pass over the list.
    orphan::drain_releasable(&EXT2_VFS_STATIC);
    let _ = ext2_vfs_sync();
    mark_filesystem_clean();
}

/// Clear the not-clean bit, so the next mount knows the image was shut down
/// in an orderly way. Runs after the final sync: a failure to flush must
/// leave the image marked dirty.
fn mark_filesystem_clean() {
    if !EXT2_VFS_INIT.is_set() {
        return;
    }
    let Ok(mut guard) = CACHED_EXT2.lock() else {
        return;
    };
    let Some(cached) = guard.as_mut() else {
        return;
    };
    if cached.cache.dirty_count() > 0
        || cached.cache.unbarriered_writes() > 0
        || cached.superblock_dirty
    {
        return;
    }
    if cached.read_only {
        return;
    }
    let (sb, bs, is) = (cached.superblock, cached.block_size, cached.inode_size);
    let Ok(mut fs) = Ext2Fs::new(&*cached.device, &mut cached.cache, sb, bs, is) else {
        return;
    };
    if fs.mark_clean().is_ok() {
        cached.superblock = fs.superblock();
        cached.superblock_dirty = fs.superblock_dirty();
    }
}

fn start_flusher() {
    if !FLUSH_THREAD_STARTED.init_once() {
        return;
    }
    if slopos_ostd::spawn_kernel_io!(&FLUSH_STOP, ext2_flusher_entry).is_err() {
        // Roll back so a later mount can retry the spawn; eviction, `sync` and
        // shutdown still persist without it.
        FLUSH_THREAD_STARTED.reset();
    }
}

/// Background writeback kthread (analog of Linux per-bdi flusher).
///
/// A failed sync leaves blocks dirty, which would satisfy the wait predicate
/// immediately and retry back-to-back forever, so persistent failures back off
/// exponentially and ignore dirty-counter wakes until the backoff elapses.
fn ext2_flusher_entry(token: KernelIoToken<'static>) {
    let mut backoff_ms: u64 = 0;
    loop {
        let waited = if backoff_ms > 0 {
            token.park_timeout(&FLUSH_STOP, || false, backoff_ms)
        } else {
            token.park_timeout(
                &FLUSH_STOP,
                || DIRTY_PENDING.load(Ordering::Relaxed) > 0 || orphan::releasable_count() > 0,
                FLUSH_INTERVAL_MS,
            )
        };

        // Before the sync, so the frees it performs go out in the same pass
        // rather than waiting a further tick. Takes the mount lock itself, so
        // it must not run under one.
        orphan::drain_releasable(&EXT2_VFS_STATIC);

        // Sync on the stop path too: dirty blocks that never reach the device
        // are lost.
        let result = ext2_vfs_sync();
        backoff_ms = if result.is_err() {
            (backoff_ms * 2).clamp(FLUSH_BACKOFF_MIN_MS, FLUSH_BACKOFF_MAX_MS)
        } else {
            0
        };
        if waited == KthreadWait::Stop {
            break;
        }
    }
    FLUSH_STOP.note_exited();
}

pub fn ext2_vfs_is_initialized() -> bool {
    EXT2_VFS_INIT.is_set()
}

fn ext2_error_to_vfs(e: Ext2Error) -> VfsError {
    match e {
        Ext2Error::InvalidSuperblock => VfsError::IoError,
        Ext2Error::UnsupportedBlockSize => VfsError::IoError,
        Ext2Error::UnsupportedFeature => VfsError::NotSupported,
        Ext2Error::ReadOnly => VfsError::ReadOnly,
        Ext2Error::InvalidInode => VfsError::NotFound,
        Ext2Error::InvalidBlock => VfsError::IoError,
        // The caller's argument, not the image: `EINVAL`, and no latch.
        Ext2Error::InvalidRange => VfsError::InvalidArgument,
        Ext2Error::UnsupportedIndirection => VfsError::NotSupported,
        Ext2Error::DeviceError => VfsError::IoError,
        Ext2Error::DirectoryFormat => VfsError::IoError,
        Ext2Error::NotDirectory => VfsError::NotDirectory,
        Ext2Error::NotFile => VfsError::NotFile,
        Ext2Error::PathNotFound => VfsError::NotFound,
        Ext2Error::NoSpace => VfsError::NoSpace,
        Ext2Error::NameTooLong => VfsError::NameTooLong,
        Ext2Error::AlreadyExists => VfsError::AlreadyExists,
        Ext2Error::NotEmpty => VfsError::NotEmpty,
        Ext2Error::IsDirectory => VfsError::IsDirectory,
        Ext2Error::TooManyLinks => VfsError::TooManyLinks,
        Ext2Error::OutOfMemory => VfsError::IoError,
        Ext2Error::Immutable => VfsError::PermissionDenied,
        Ext2Error::InvalidPath => VfsError::InvalidPath,
    }
}

fn inode_to_file_type(inode: &Ext2Inode) -> FileType {
    let mode = inode.mode & 0xF000;
    match mode {
        0x4000 => FileType::Directory,
        0x8000 => FileType::Regular,
        0xA000 => FileType::Symlink,
        0x2000 => FileType::CharDevice,
        0x6000 => FileType::BlockDevice,
        0x1000 => FileType::Pipe,
        0xC000 => FileType::Socket,
        _ => FileType::Regular,
    }
}

fn ext2_file_type_to_vfs(file_type: u8) -> FileType {
    match file_type {
        1 => FileType::Regular,
        2 => FileType::Directory,
        3 => FileType::CharDevice,
        4 => FileType::BlockDevice,
        5 => FileType::Pipe,
        6 => FileType::Socket,
        7 => FileType::Symlink,
        _ => FileType::Regular,
    }
}

struct Ext2CacheReclaim;

impl slopos_ostd::mm::reclaim::Reclaimable for Ext2CacheReclaim {
    fn name(&self) -> &'static str {
        "ext2-page-cache"
    }

    fn reclaimable_pages(&self) -> u32 {
        // `try_lock` only: the FS lock is a sleeping mutex held across block
        // I/O, so waiting here blocks on the I/O that needs the memory.
        let Some(guard) = CACHED_EXT2.try_lock() else {
            return 0;
        };
        guard
            .as_ref()
            .map_or(0, |cached| cached.cache.reclaimable())
    }

    fn reclaim(&self, want: u32) -> u32 {
        let Some(mut guard) = CACHED_EXT2.try_lock() else {
            return 0;
        };
        guard
            .as_mut()
            .map_or(0, |cached| cached.cache.shrink_clean(want))
    }
}

static EXT2_CACHE_RECLAIM: Ext2CacheReclaim = Ext2CacheReclaim;

pub fn register_reclaim(token: &slopos_ostd::sync::BspToken<'_>) {
    slopos_ostd::mm::reclaim::register(token, &EXT2_CACHE_RECLAIM);
}
