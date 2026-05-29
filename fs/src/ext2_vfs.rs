use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::blockdev::BlockDevice;
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode, Ext2Superblock};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_ostd::KBox;
use slopos_ostd::sync::kernel_io_task::KernelIoToken;
use slopos_ostd::sync::wait_queue::WaitQueue;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, PreemptMutex};

const EXT2_ROOT_INODE: u32 = 2;

// ============================================================================
// Persistent mounted-filesystem state.
//
// Both the superblock geometry AND the write-back buffer cache live here for
// the lifetime of the mount. The cache is *not* rebuilt per call: the thin,
// allocation-free `Ext2Fs` handle borrows it inside `with_fs`. This is what
// makes durability structural — dirty blocks accumulate in a cache that is
// never dropped, so a missed flush can no longer lose data; it is persisted
// later by eviction, the background flusher, an explicit `sync`, or shutdown.
// ============================================================================

struct CachedExt2 {
    /// The filesystem's sole writable handle to its backing block device —
    /// an owned capability (e.g. a virtio-blk `BlockWriteToken`) boxed behind
    /// `dyn BlockDevice`. Held for the kernel's lifetime, so no other code can
    /// acquire a second writer to this device (Layer 1: ownership = exclusion).
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    /// The persistent write-back buffer cache, sized to `block_size` at mount.
    cache: BlockCache,
}

static CACHED_EXT2: PreemptMutex<Option<CachedExt2>> = PreemptMutex::new(None, LOCK_LEVEL_RESOURCE);
static EXT2_VFS_INIT: InitFlag = InitFlag::new();

// ---- Background writeback (flusher) plumbing ----

/// Best-effort count of dirty cache blocks awaiting writeback, published by
/// each mutating op and the flusher. The flusher's wait predicate reads only
/// this (and `FLUSH_SHUTDOWN`) — it performs no I/O and takes no lock, so it
/// stays a pure observer (`check_wait_predicate_purity.sh`).
static DIRTY_PENDING: AtomicUsize = AtomicUsize::new(0);
static FLUSH_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static FLUSH_WQ: WaitQueue = WaitQueue::new();
static FLUSH_THREAD_STARTED: InitFlag = InitFlag::new();

/// Periodic writeback cadence — analog of Linux `dirty_writeback_centisecs`
/// (5 s). Dirty blocks never linger longer than this between syncs.
const FLUSH_INTERVAL_MS: u64 = 5_000;
/// Eager-wake threshold — analog of `dirty_background_ratio`. Past this many
/// dirty blocks a mutating op kicks the flusher immediately rather than waiting
/// for the periodic tick.
const FLUSH_EAGER_THRESHOLD: usize = 48;

pub struct StaticExt2Vfs;

impl StaticExt2Vfs {
    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        if !EXT2_VFS_INIT.is_set() {
            return Err(VfsError::IoError);
        }
        let mut guard = CACHED_EXT2.lock();
        let cached = guard.as_mut().ok_or(VfsError::IoError)?;
        let (superblock, block_size, inode_size) =
            (cached.superblock, cached.block_size, cached.inode_size);
        // Borrow the persistent cache and device disjointly; building the
        // handle allocates nothing.
        let mut fs = Ext2Fs::new(
            &*cached.device,
            &mut cached.cache,
            superblock,
            block_size,
            inode_size,
        );
        let result = f(&mut fs).map_err(ext2_error_to_vfs);
        let new_superblock = fs.superblock();
        let dirty = fs.dirty_count();
        drop(fs);

        // Write-back policy: do NOT synchronously flush per op. Dirty blocks
        // stay in the persistent cache and are written back by eviction, the
        // background flusher, an explicit `sync`, or shutdown. Durability is no
        // longer hostage to remembering a per-call flush.
        if result.is_ok() {
            // Publish the in-memory superblock changes from THIS op only on
            // success; a failed op must not leak free-count drift into the
            // live superblock.
            cached.superblock = new_superblock;
            note_dirty(dirty);
        }
        // NOTE (crash/failure atomicity): on the error path we leave any blocks
        // the failed op already dirtied in the persistent cache (we do not
        // track per-op write sets, so we cannot selectively undo them without
        // dropping other ops' legitimately-dirty blocks). A later sync may thus
        // persist partial metadata from a failed op (e.g. an allocated-but-
        // unreferenced inode), which is fsck-recoverable free-count/orphan
        // drift — the exact class a write-ahead journal (jbd2-style) would make
        // atomic. Adding that journal is the documented next durability layer;
        // see the crate notes. Validation-only failures (the common case)
        // mutate nothing and are unaffected.
        result
    }
}

/// Publish the dirty-block count and, if it crosses the eager threshold, kick
/// the background flusher. Never holds the FS lock.
fn note_dirty(dirty: usize) {
    DIRTY_PENDING.store(dirty, Ordering::Relaxed);
    if dirty >= FLUSH_EAGER_THRESHOLD {
        FLUSH_WQ.wake_one();
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

pub fn ext2_vfs_init_with_device(device: KBox<dyn BlockDevice + Send + Sync>) -> VfsResult<()> {
    // Atomically claim the one-shot init. A second call returns an error rather
    // than silently dropping the caller's capability token (which would release
    // the exclusive write claim) while falsely reporting success.
    if !EXT2_VFS_INIT.init_once() {
        return Err(VfsError::AlreadyExists);
    }

    // Wrap the device in a read-time integrity verifier if the image carries a
    // verity trailer (no-op otherwise). Corruption of a not-yet-written block
    // then fails the read loudly instead of returning bad bytes.
    let device = crate::verity::build_verified(device);

    // Validate the filesystem against the owned device. On failure, roll the
    // one-shot flag back so it stays in lockstep with `CACHED_EXT2`
    // (`is_set()` ⟺ a filesystem is actually mounted) and a later attempt can
    // retry. The owned `device` drops here, releasing its (failed) claim.
    let (superblock, block_size, inode_size) = match Ext2Fs::mount_params(&*device) {
        Ok(parts) => parts,
        Err(e) => {
            EXT2_VFS_INIT.reset();
            return Err(ext2_error_to_vfs(e));
        }
    };

    // Build the persistent buffer cache sized to this filesystem's block size.
    let cache = match BlockCache::new(block_size) {
        Ok(c) => c,
        Err(e) => {
            EXT2_VFS_INIT.reset();
            return Err(ext2_error_to_vfs(e));
        }
    };

    let mut guard = CACHED_EXT2.lock();
    *guard = Some(CachedExt2 {
        device,
        superblock,
        block_size,
        inode_size: if inode_size == 0 { 128 } else { inode_size },
        cache,
    });
    drop(guard);

    // Start the background writeback flusher once a filesystem is mounted.
    // Spawn failures are non-fatal: eviction + explicit `sync` + shutdown still
    // provide durability, just without periodic background writeback.
    start_flusher();

    Ok(())
}

/// Flush all dirty cached blocks and the superblock to the backing device with
/// ordered durability barriers (see [`Ext2Fs::sync`]). Safe to call from any
/// context that may hold no FS lock (the flusher thread, `vfs` sync, kernel
/// shutdown). A no-op if no ext2 filesystem is mounted.
pub fn ext2_vfs_sync() -> VfsResult<()> {
    if !EXT2_VFS_INIT.is_set() {
        return Ok(());
    }
    let mut guard = CACHED_EXT2.lock();
    let Some(cached) = guard.as_mut() else {
        return Ok(());
    };
    let (superblock, block_size, inode_size) =
        (cached.superblock, cached.block_size, cached.inode_size);
    let mut fs = Ext2Fs::new(
        &*cached.device,
        &mut cached.cache,
        superblock,
        block_size,
        inode_size,
    );
    let result = fs.sync().map_err(ext2_error_to_vfs);
    let dirty = fs.dirty_count();
    drop(fs);
    DIRTY_PENDING.store(dirty, Ordering::Relaxed);
    result
}

/// Request a final synchronous writeback at kernel shutdown. Must be called
/// while interrupts are still enabled (the virtio-blk completion path needs
/// IRQs) — see `boot::shutdown::kernel_shutdown`. Best-effort.
pub fn ext2_vfs_shutdown_sync() {
    FLUSH_SHUTDOWN.store(true, Ordering::Relaxed);
    FLUSH_WQ.wake_all();
    let _ = ext2_vfs_sync();
}

fn start_flusher() {
    if !FLUSH_THREAD_STARTED.init_once() {
        return;
    }
    if slopos_ostd::spawn_kernel_io!("ext2-flush", ext2_flusher_entry).is_err() {
        // Roll back so a later mount attempt can retry the spawn; durability
        // is unaffected (eviction/sync/shutdown still persist).
        FLUSH_THREAD_STARTED.reset();
    }
}

/// Background writeback kthread (analog of Linux per-bdi flusher).
///
/// Sleeps up to [`FLUSH_INTERVAL_MS`], or wakes early when a mutating op
/// crosses [`FLUSH_EAGER_THRESHOLD`] dirty blocks, then performs one ordered
/// [`ext2_vfs_sync`]. Runs at `TaskPriority::KernelIo` (above user tasks) so
/// writeback keeps up even under user load. The wait predicate only observes
/// atomics — no I/O, no locks — keeping it pure.
fn ext2_flusher_entry(_token: KernelIoToken<'static>) {
    loop {
        FLUSH_WQ.wait_event_timeout(
            || DIRTY_PENDING.load(Ordering::Relaxed) > 0 || FLUSH_SHUTDOWN.load(Ordering::Relaxed),
            FLUSH_INTERVAL_MS,
        );
        let shutting_down = FLUSH_SHUTDOWN.load(Ordering::Relaxed);
        let _ = ext2_vfs_sync();
        if shutting_down {
            break;
        }
    }
}

pub fn ext2_vfs_is_initialized() -> bool {
    EXT2_VFS_INIT.is_set()
}

// ============================================================================
// Helpers
// ============================================================================

fn ext2_error_to_vfs(e: Ext2Error) -> VfsError {
    match e {
        Ext2Error::InvalidSuperblock => VfsError::IoError,
        Ext2Error::UnsupportedBlockSize => VfsError::IoError,
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
