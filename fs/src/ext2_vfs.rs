use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

use crate::blockdev::BlockDevice;
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode, Ext2Superblock};
use crate::verity::{FsExtent, VerityError, VerityStatus};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
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
}

/// How the device came up at mount, for the boot log and the mounter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext2MountInfo {
    pub verity: VerityStatus,
    pub read_only: bool,
}

/// A *sleeping* mutex: ext2 block-device I/O waits are scheduler-backed, so
/// the holder may legitimately deschedule mid-operation.
static CACHED_EXT2: Mutex<Option<CachedExt2>> =
    Mutex::new(None, lock_class!("CACHED_EXT2", LOCK_LEVEL_RESOURCE));
static EXT2_VFS_INIT: InitFlag = InitFlag::new();
/// Every `Ext2Fs` built over the mounted device refuses mutation. Outside the
/// lock so a query never waits on in-flight block I/O.
static EXT2_READ_ONLY: AtomicBool = AtomicBool::new(false);

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
        let result = f(&mut fs).map_err(ext2_error_to_vfs);
        let new_superblock = fs.superblock();
        let new_superblock_dirty = fs.superblock_dirty();
        let dirty = fs.dirty_count();
        drop(fs);

        // Deliberately no per-op flush: dirty blocks stay in the persistent
        // cache until eviction, the background flusher, `sync`, or shutdown.
        if result.is_ok() {
            // Publish superblock changes only on success: a failed op must not
            // leak free-count drift into the live superblock.
            cached.superblock = new_superblock;
            cached.superblock_dirty = new_superblock_dirty;
            note_dirty(dirty);
        }
        // TODO(tech-debt): a failed op leaves its dirtied blocks cached, so a
        // later sync can persist partial metadata — fix is a write-ahead journal.
        result
    }
}

/// Never holds the FS lock.
fn note_dirty(dirty: usize) {
    DIRTY_PENDING.store(dirty, Ordering::Relaxed);
    if dirty >= FLUSH_EAGER_THRESHOLD {
        FLUSH_STOP.wake_one_for_work();
    }
}

trait Ext2VfsBackend {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R>;
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
                // No inode-attribute mutator, so nothing can ever set it.
                sealed: false,
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file(inode as u32, offset as u32, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.write_file(inode as u32, offset as u32, buf))
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

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            if !ext2_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            let mut count = 0usize;
            let mut current = 0usize;
            fs.for_each_dir_entry(inode as u32, |entry| {
                if current < offset {
                    current += 1;
                    return true;
                }
                let ft = ext2_file_type_to_vfs(entry.file_type);
                let cont = callback(entry.name, entry.inode.raw() as InodeId, ft);
                count += 1;
                current += 1;
                cont
            })?;
            Ok(count)
        })
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn sync(&self) -> VfsResult<()> {
        ext2_vfs_sync()
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

    let read_only = Ext2Fs::read_only_for(&superblock, &*device);
    install_cached(device, superblock, block_size, inode_size)?;
    EXT2_READ_ONLY.store(read_only, Ordering::Release);

    if !read_only {
        start_flusher();
    }
    Ok(Ext2MountInfo { verity, read_only })
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
    if cached.cache.dirty_count() == 0 && !cached.superblock_dirty {
        DIRTY_PENDING.store(0, Ordering::Relaxed);
        return Ok(());
    }
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
    let result = fs.sync().map_err(ext2_error_to_vfs);
    let superblock_dirty = fs.superblock_dirty();
    let dirty = fs.dirty_count();
    drop(fs);
    cached.superblock_dirty = superblock_dirty;
    DIRTY_PENDING.store(dirty, Ordering::Relaxed);
    result
}

/// Must be called with interrupts still enabled — the virtio-blk completion
/// path needs them. Best-effort.
pub fn ext2_vfs_shutdown_sync() {
    FLUSH_STOP.request();
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
    if cached.cache.dirty_count() > 0 || cached.superblock_dirty {
        return;
    }
    let (sb, bs, is) = (cached.superblock, cached.block_size, cached.inode_size);
    let Ok(mut fs) = Ext2Fs::new(&*cached.device, &mut cached.cache, sb, bs, is) else {
        return;
    };
    if fs.mark_clean().is_ok() {
        cached.superblock = fs.superblock();
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
                || DIRTY_PENDING.load(Ordering::Relaxed) > 0,
                FLUSH_INTERVAL_MS,
            )
        };

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
