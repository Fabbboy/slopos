pub mod blockmap;
pub mod cache;
pub mod dir;
#[path = "alloc.rs"]
pub mod ext2_alloc;
pub mod file;
pub mod geometry;
pub mod inode;
pub mod journal;
pub mod ondisk;
pub mod symlink;
pub mod time;
pub mod types;

use cache::{BlockCache, BlockOwner};
use geometry::Ext2Geometry;
use ondisk::{
    DIR_FT_DIR, DIR_FT_REG_FILE, DIR_FT_SYMLINK, DirEntry, EXT2_ERROR_FS, EXT2_IMMUTABLE_FL,
    EXT2_VALID_FS, GroupDesc, Inode, MODE_DIRECTORY, MODE_FILE, MODE_PERM_MASK,
    RO_COMPAT_LARGE_FILE, S_LAST_ORPHAN_OFF, Superblock,
};
use types::{BlockNum, FileBlock, GroupIdx, InodeNum};

use crate::blockdev::BlockDevice;
use slopos_ostd::KVec;

pub use ondisk::EXT2_MAX_BLOCK_SIZE;

/// Where the metadata log lives. A plain preallocated file, so an image that
/// has none simply has none and `e2fsck` needs to know nothing about it.
pub const JOURNAL_PATH: &[u8] = b"/.journal";

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Ext2Error {
    InvalidSuperblock,
    UnsupportedBlockSize,
    /// The image declares an incompatible feature this implementation cannot
    /// represent; mounting it read-write would corrupt it.
    UnsupportedFeature,
    /// The mount is read-only, either by request or because the image carries
    /// an unsupported read-only-compatible feature.
    ReadOnly,
    InvalidInode,
    /// A block number the *image* names is outside the volume, or a structure
    /// the image owns failed its own validity check. Evidence of damage, so
    /// [`Ext2Error::is_corruption`] latches the mount on it.
    InvalidBlock,
    /// A *caller-supplied* offset, cursor or argument does not address
    /// anything valid.
    ///
    /// Split from [`Self::InvalidBlock`] precisely because that variant is
    /// evidence of a damaged image and this one is not: `readdir_cookie`'s
    /// cursor comes from userland through `fs_list`, so an argument error that
    /// latched the mount would let any process take the filesystem read-only
    /// for everybody with one bad `u64`.
    InvalidRange,
    UnsupportedIndirection,
    DeviceError,
    DirectoryFormat,
    NotDirectory,
    NotFile,
    PathNotFound,
    NoSpace,
    NameTooLong,
    AlreadyExists,
    NotEmpty,
    IsDirectory,
    TooManyLinks,
    OutOfMemory,
    /// The inode carries `EXT2_IMMUTABLE_FL` and refuses every mutation.
    Immutable,
    /// A rename would splice a directory into its own subtree, detaching it
    /// and everything under it from the root.
    InvalidPath,
}

impl Ext2Error {
    /// Whether this error means the *image or the device* is wrong, rather
    /// than the caller.
    ///
    /// This is what `errors=remount-ro` keys on, so the classification has to
    /// be conservative in one specific direction: an error a caller can
    /// produce on demand must never appear here, or an unprivileged
    /// `stat`/`unlink` of a nonexistent thing would flip the whole mount
    /// read-only. `InvalidInode` is the sharp case — it covers both "that
    /// inode number is out of range", which any caller can ask for, and a
    /// genuinely damaged group descriptor — so it stays out. The four below
    /// are only ever produced by a structure that failed its own validity
    /// check, or by the device refusing I/O.
    pub fn is_corruption(self) -> bool {
        matches!(
            self,
            Self::DeviceError
                | Self::InvalidBlock
                | Self::DirectoryFormat
                | Self::InvalidSuperblock
        )
    }
}

/// Why a mount refuses every mutation. Carried to the boot log and to the
/// mount flags, so `EROFS` at the VFS has a stated cause rather than being an
/// unexplained property of the disk.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// A verity trailer makes the device refuse writes.
    DeviceWriteProtected,
    /// The image declares a read-only-compatible feature this implementation
    /// does not write.
    UnsupportedFeature,
    /// `s_state` is not `EXT2_VALID_FS`: the last mount never marked it
    /// clean. Repair with `e2fsck` on the host.
    NotCleanlyUnmounted,
    /// An operation found the image or the device damaged after mount
    /// (`errors=remount-ro`).
    ErrorsRemountRo,
}

pub type Ext2Superblock = Superblock;
pub type Ext2Inode = Inode;

/// What `create_inode_entry` is being asked to build.
#[derive(Copy, Clone)]
enum NewInode<'a> {
    File,
    Directory,
    Symlink(&'a [u8]),
}

/// Which of `unlink(2)` and `rmdir(2)` a removal is, so the wrong one on a
/// directory reports `EISDIR`/`ENOTDIR` instead of silently doing the other.
#[derive(Copy, Clone, PartialEq, Eq)]
enum RemoveKind {
    NonDirectory,
    Directory,
}

/// What removing the *last* name of an inode does with the inode itself.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LastLink {
    /// Free the blocks and the inode now.
    Free,
    /// Keep both, and thread the inode onto the on-disk orphan list: a
    /// descriptor still holds it, and POSIX says its contents survive until
    /// the last close.
    Orphan,
}

/// A validated rename, carrying only what the mutation stages need — not the
/// `Inode`s the validation read, which is what keeps each stage's frame small.
struct RenamePlan {
    source: InodeNum,
    source_is_dir: bool,
    file_type: u8,
    /// Set when the destination name exists and must be removed first.
    displaced: Option<RemoveKind>,
    /// The two ends name different directories, so a moved directory shifts a
    /// `..` link between them.
    reparenting: bool,
}

/// The directory-entry type byte matching an inode's mode. The `filetype`
/// incompat feature is negotiated, so an entry carrying the wrong byte is a
/// lookup that answers with the wrong `d_type` for every other reader.
fn dir_file_type(inode: &Inode) -> u8 {
    use ondisk::{
        DIR_FT_BLKDEV, DIR_FT_CHRDEV, DIR_FT_FIFO, DIR_FT_SOCK, DIR_FT_UNKNOWN, MODE_BLOCKDEV,
        MODE_CHARDEV, MODE_FIFO, MODE_SOCKET,
    };
    match inode.file_type_mode() {
        MODE_DIRECTORY => DIR_FT_DIR,
        MODE_FILE => DIR_FT_REG_FILE,
        ondisk::MODE_SYMLINK => DIR_FT_SYMLINK,
        MODE_CHARDEV => DIR_FT_CHRDEV,
        MODE_BLOCKDEV => DIR_FT_BLKDEV,
        MODE_FIFO => DIR_FT_FIFO,
        MODE_SOCKET => DIR_FT_SOCK,
        _ => DIR_FT_UNKNOWN,
    }
}

/// Which ordered phase a writeback pass is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPhase {
    Data,
    Metadata,
    Logged,
    Superblock,
    Done,
}

/// A whole-filesystem writeback pass, resumable across mount-lock releases.
///
/// Carrying the epoch, the log head and the log generation it opened on is
/// what bounds the pass: an operation that runs between two steps is entirely
/// outside it, so the resumed pass can neither publish that operation's
/// metadata ahead of its data nor empty a log that has grown — or been emptied
/// and refilled — behind it.
#[derive(Debug, Clone, Copy)]
pub struct SyncPass {
    epoch: u64,
    phase: SyncPhase,
    cursor: u32,
    limit: u32,
    /// Which emptying of the log `cursor` and `limit` index. Another pass can
    /// reset and refill it between two steps, after which those indices name
    /// someone else's records.
    generation: u32,
}

impl SyncPass {
    pub fn is_done(&self) -> bool {
        self.phase == SyncPhase::Done
    }
}

/// RAII scope for one all-or-nothing ext2 operation.
///
/// Rolls back on drop unless [`Self::commit`] ran, which covers the `?` exits
/// that make up most of the failure paths below as well as an explicit
/// `return Err`. The `Drop` is panic-free by construction: every step it takes
/// is a field assignment or a `BlockCache` method that cannot fail.
struct Ext2Txn<'t, 'a> {
    fs: &'t mut Ext2Fs<'a>,
    superblock: Superblock,
    superblock_dirty: bool,
    /// This scope opened the transaction, so its snapshot is the committed
    /// state and its rollback is the one that runs.
    outermost: bool,
    committed: bool,
}

impl<'t, 'a> Ext2Txn<'t, 'a> {
    fn begin(fs: &'t mut Ext2Fs<'a>) -> Self {
        // Only the outermost scope owns the superblock snapshot, matching
        // `BlockCache::begin_op`'s own depth rule. An inner scope that
        // restored its own snapshot while the outer one went on to commit
        // would leave the free counts disagreeing with the bitmaps the cache
        // still holds.
        let outermost = !fs.in_transaction;
        let superblock = fs.superblock;
        let superblock_dirty = fs.superblock_dirty;
        fs.in_transaction = true;
        fs.cache.begin_op();
        Self {
            fs,
            superblock,
            superblock_dirty,
            outermost,
            committed: false,
        }
    }

    /// Publishing the transaction can fail on the device, and a commit that
    /// did not reach the log is not a commit: leaving `committed` clear hands
    /// the failure to [`Drop`], which rolls the operation back.
    fn commit(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.fs.cache.commit_op(device)?;
        self.committed = true;
        if self.outermost && self.fs.take_pending_orphan_head().is_some() {
            // After the commit, never before: the head may only name a
            // member whose record is already recoverable. Best-effort, as the
            // rollback's compensation is — failing leaves a list shorter than
            // the truth, which `e2fsck` reclaims, not a chain that lies.
            if self.fs.write_orphan_head_now().is_err() {
                self.fs.corruption_seen = true;
            }
        }
        Ok(())
    }
}

impl Drop for Ext2Txn<'_, '_> {
    fn drop(&mut self) {
        if self.committed {
            if self.outermost {
                self.fs.in_transaction = false;
            }
            return;
        }
        self.fs.cache.rollback_op();
        if self.outermost {
            // The orphan-list head is the one field an operation publishes
            // straight to the device, so the cache rollback above cannot
            // retract it. Put the committed value back before the in-memory
            // superblock is restored, or the two disagree: on disk the head
            // names an inode whose zeroing has just been rolled back, and a
            // crash before the next `sync` gives the next mount's drain a head
            // it refuses — which then discards the genuine chain behind it.
            //
            // Best-effort by necessity: this is a destructor, so a device
            // error has nowhere to go. Failing leaves exactly the state that
            // not trying would have, and `e2fsck` reclaims it.
            // Under a log the head write was deferred past a commit that
            // never happened, so there is nothing on the device to retract.
            let deferred = self.fs.take_pending_orphan_head().is_some();
            if !deferred && self.fs.superblock.last_orphan != self.superblock.last_orphan {
                let published = self.fs.superblock.last_orphan;
                self.fs.superblock.last_orphan = self.superblock.last_orphan;
                let _ = self.fs.write_orphan_head();
                self.fs.superblock.last_orphan = published;
            }
            self.fs.superblock = self.superblock;
            self.fs.superblock_dirty = self.superblock_dirty;
            self.fs.in_transaction = false;
        }
    }
}

/// Mutating entry points below carry `#[inline(never)]`. Each holds one or
/// two `Inode`s live along with a directory helper's own frame, and inlining
/// several into one caller sums past the 2 KiB stack gate.
pub struct Ext2Fs<'a> {
    device: &'a dyn BlockDevice,
    superblock: Superblock,
    geom: Ext2Geometry,
    cache: &'a mut BlockCache,
    block_size: u32,
    inode_size: u16,
    ptrs_per_block: u32,
    /// Set when an op changed the in-memory free-counts, leaving the on-disk
    /// superblock stale; persisted only by [`Self::sync`], never mid-operation.
    superblock_dirty: bool,
    /// Refuses every mutating entry point. Set when the image declares a
    /// read-only-compatible feature this implementation does not write, or
    /// when the device itself refuses writes (a verity-attested image).
    read_only: bool,
    /// An operation on this handle hit an error that means the image or the
    /// device is damaged. The owner of the mount reads this back and latches
    /// the mount read-only — `errors=remount-ro`.
    corruption_seen: bool,
    /// A [`Ext2Txn`] scope is open, so a nested one must not take a second
    /// superblock snapshot.
    in_transaction: bool,
    /// `s_last_orphan` an open transaction moved, owed to the device once it
    /// commits. Only set under a log, which is what makes deferring it safe:
    /// the member's record reaches the log before the head names it.
    pending_orphan_head: Option<u32>,
    /// The log's own file, kept even when no log was attached: the reason to
    /// refuse a reader is the file's contents, not whether this boot logs.
    journal_inode: Option<u32>,
}

impl<'a> Ext2Fs<'a> {
    /// Reads the superblock *without* a cache: mount needs `block_size` to size
    /// the [`BlockCache`] before it exists. Returns
    /// `(superblock, block_size, inode_size)`.
    ///
    /// `#[inline(never)]`, as with the other two superblock-I/O helpers: each
    /// stages the full 1024-byte block on its own frame, and three of those
    /// inlined into one caller is 3 KiB of the 2 KiB budget.
    #[inline(never)]
    pub fn mount_params(device: &dyn BlockDevice) -> Result<(Superblock, u32, u16), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        let superblock = Superblock::parse(&sb_buf)?;
        let block_size = superblock.block_size()?;
        let inode_size = superblock.effective_inode_size();
        Ok((superblock, block_size, inode_size))
    }

    /// `s_r_blocks_count`, read straight off the device.
    ///
    /// Its own read rather than a fourth element on [`Self::mount_params`]'s
    /// tuple, which every other caller would have to name and discard.
    /// `#[inline(never)]` for the same 1 KiB-buffer reason.
    #[inline(never)]
    pub fn read_block_reserve(device: &dyn BlockDevice) -> Result<u32, Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(ondisk::reserved_blocks_of(&sb_buf))
    }

    /// **Performs no allocation** — the cache lives in the long-lived FS state
    /// and is merely borrowed, so building one of these per VFS call is free.
    pub fn new(
        device: &'a dyn BlockDevice,
        cache: &'a mut BlockCache,
        superblock: Superblock,
        block_size: u32,
        inode_size: u16,
    ) -> Result<Self, Ext2Error> {
        let read_only = Self::read_only_for(&superblock, device);
        let geom = Ext2Geometry::derive(&superblock)?;
        Ok(Self {
            device,
            superblock,
            geom,
            cache,
            block_size,
            inode_size,
            ptrs_per_block: block_size / 4,
            superblock_dirty: false,
            read_only,
            corruption_seen: false,
            in_transaction: false,
            pending_orphan_head: None,
            journal_inode: None,
        })
    }

    pub fn geometry(&self) -> &Ext2Geometry {
        &self.geom
    }

    /// Hold back `reserved` blocks from unprivileged allocation
    /// (`s_r_blocks_count`).
    ///
    /// Per call rather than per mount: entitlement is a property of who is
    /// asking, and the mount is shared by every process on it.
    pub fn set_block_reserve(&mut self, reserved: u32) {
        self.geom = self.geom.with_reserve(reserved);
    }

    /// Charge block allocations through this handle to `account`.
    ///
    /// Per call, for the same reason the reserve is: the mount is shared and
    /// the allocation is the caller's.
    pub fn set_account(&mut self, account: slopos_ostd::process::AccountId) {
        self.geom = self.geom.with_account(account);
    }

    /// The log's file, for the mount to carry across handles.
    pub fn journal_inode(&self) -> Option<u32> {
        self.journal_inode
    }

    /// Refuse readers of the log's file on this handle. Per call, like the
    /// reserve and the account: the value belongs to the mount.
    pub fn set_journal_inode(&mut self, ino: Option<u32>) {
        self.journal_inode = ino;
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The one rule for whether a handle over `device` may mutate.
    ///
    /// Deliberately does **not** consult `s_state`: a mounted image carries
    /// `EXT2_ERROR_FS` by design, so a per-call handle built after
    /// [`Self::mark_dirty_on_disk`] would read its own mount stamp as damage
    /// and refuse every write. The not-cleanly-unmounted question is asked
    /// once, at mount, against the superblock as it came off the disk — see
    /// [`Self::mount_read_only_reason`].
    pub fn read_only_for(superblock: &Superblock, device: &dyn BlockDevice) -> bool {
        superblock.requires_readonly() || device.write_protected()
    }

    /// Why a mount of `superblock` over `device` must refuse writes, if it
    /// must. `superblock` has to be the one read at mount, before this
    /// implementation stamped anything into it.
    pub fn mount_read_only_reason(
        superblock: &Superblock,
        device: &dyn BlockDevice,
    ) -> Option<ReadOnlyReason> {
        if device.write_protected() {
            return Some(ReadOnlyReason::DeviceWriteProtected);
        }
        if superblock.requires_readonly() {
            return Some(ReadOnlyReason::UnsupportedFeature);
        }
        // The image was never marked clean, so a previous mount either is
        // still running or died mid-write. Either way the free counts, the
        // bitmaps and the inode table may disagree with each other, and
        // writing into that turns a repairable image into a lost one. There
        // is no in-kernel fsck and there should not be one: read-only plus a
        // loud log line is the honest behaviour, and `e2fsck` is the repair.
        if superblock.state != EXT2_VALID_FS {
            return Some(ReadOnlyReason::NotCleanlyUnmounted);
        }
        None
    }

    /// Latch this handle read-only for a reason its constructor could not see
    /// — a mount refused for [`ReadOnlyReason::NotCleanlyUnmounted`], or a
    /// mount already flipped by `errors=remount-ro`.
    pub fn force_read_only(&mut self) {
        self.read_only = true;
    }

    /// Whether an operation on this handle saw evidence that the image or the
    /// device is damaged. The mount owner latches on this.
    pub fn corruption_seen(&self) -> bool {
        self.corruption_seen
    }

    /// Record a corruption verdict on the way out of an operation.
    ///
    /// Called once, by whoever owns the mount, around the whole operation —
    /// rather than at each entry point, which is a list that the next entry
    /// point added can be left off. `transaction` classifies too, because a
    /// rollback needs the verdict before the error leaves the scope, and
    /// setting the same flag twice is idempotent.
    pub fn note_result<R>(&mut self, result: Result<R, Ext2Error>) -> Result<R, Ext2Error> {
        if let Err(e) = &result
            && e.is_corruption()
        {
            self.corruption_seen = true;
        }
        result
    }

    /// Gate for every mutating entry point.
    fn check_writable(&self) -> Result<(), Ext2Error> {
        if self.read_only {
            return Err(Ext2Error::ReadOnly);
        }
        Ok(())
    }

    /// Run `f` as one all-or-nothing operation: an error rolls back every
    /// block it dirtied and every free-count it moved, so a later flush cannot
    /// publish half of it.
    ///
    /// With a redo log the retraction is exact, because no home block carries
    /// the operation's changes until its commit record is on the medium.
    /// Without one the scope is cache-deep: a block evicted mid-operation
    /// reached its home before the failure, and
    /// [`BlockCache::find_or_evict`] only makes that the case of last resort.
    fn transaction<R>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<R, Ext2Error>,
    ) -> Result<R, Ext2Error> {
        // The only point at which the log can be emptied: a check point
        // publishes committed content, and mid-operation the same blocks hold
        // uncommitted content.
        if !self.in_transaction && !self.cache.journal_has_headroom() {
            self.checkpoint_journal()?;
        }
        let mut txn = Ext2Txn::begin(self);
        let result = f(txn.fs);
        if let Err(e) = &result
            && e.is_corruption()
        {
            txn.fs.corruption_seen = true;
        }
        // A scope whose undo record overflowed can no longer be undone, so
        // committing it would publish work the guard can no longer stand
        // behind. Failing here rolls back what is still recorded, which is a
        // consistent prefix, and the caller sees `NoSpace`.
        if result.is_ok() && txn.fs.cache.op_undo_overflowed() {
            return Err(Ext2Error::NoSpace);
        }
        if result.is_ok() {
            let device = txn.fs.device;
            txn.commit(device)?;
        }
        result
    }

    /// Mark the image as not cleanly unmounted, so a later fsck knows it must
    /// run, and record the mount in the fields `e2fsck` reports.
    pub fn mark_dirty_on_disk(&mut self) -> Result<(), Ext2Error> {
        if self.read_only || self.superblock.state == EXT2_ERROR_FS {
            return Ok(());
        }
        self.superblock.state = EXT2_ERROR_FS;
        self.write_superblock_state(true)
    }

    pub fn mark_clean(&mut self) -> Result<(), Ext2Error> {
        if self.read_only {
            return Ok(());
        }
        self.superblock.state = EXT2_VALID_FS;
        self.write_superblock_state(false)
    }

    /// The bookkeeping fields as they stand on the device.
    ///
    /// Read back rather than held: they move only at mount and in the
    /// superblock write, and carrying them on this handle would put them in
    /// every operation's frame and every transaction snapshot.
    #[inline(never)]
    pub fn read_bookkeeping(&self) -> Result<ondisk::SuperblockBookkeeping, Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(ondisk::SuperblockBookkeeping::parse(&sb_buf))
    }

    /// Barriered on the spot rather than left to a later `sync`: this is a
    /// sub-block write straight to the device, so it dirties nothing and is
    /// invisible to the cache's own accounting. The clean stamp in particular
    /// is the last write before power-off, and one left in a volatile device
    /// cache means an orderly shutdown still reads as a crash.
    #[inline(never)]
    fn write_superblock_state(&mut self, mounting: bool) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.superblock.encode_mutable_fields(&mut sb_buf);
        sb_buf[58..60].copy_from_slice(&self.superblock.state.to_le_bytes());
        let now = time::now_unix_opt();
        if mounting {
            ondisk::SuperblockBookkeeping::stamp_mount(&mut sb_buf, now);
        } else {
            ondisk::SuperblockBookkeeping::stamp_write(&mut sb_buf, now);
        }
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.superblock_dirty = false;
        self.device_barrier()
    }

    pub fn superblock(&self) -> Superblock {
        self.superblock
    }

    pub fn superblock_dirty(&self) -> bool {
        self.superblock_dirty
    }

    /// The flag must survive across handles: a mutating op dirties it in one
    /// `with_fs` call and the flusher persists it from a *different* handle.
    pub fn set_superblock_dirty(&mut self, dirty: bool) {
        self.superblock_dirty = dirty;
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn dirty_count(&self) -> usize {
        self.cache.dirty_count()
    }

    /// A caller deciding whether a sync has work must consult this as well as
    /// [`Self::dirty_count`], or a clean cache reads as a durable one.
    pub fn unbarriered_writes(&self) -> usize {
        self.cache.unbarriered_writes()
    }

    /// Write dirty *data* blocks back with no barrier, as eviction does, while
    /// leaving metadata dirty — the state a durability test must be able to
    /// construct on purpose. Data only, because evicting the inode record too
    /// would leave nothing for the ordering barrier to order.
    #[cfg(feature = "tests")]
    pub fn cache_evict_data_for_test(&mut self) -> Result<(), Ext2Error> {
        self.cache
            .flush_where(self.device, |kind, _| kind == cache::BlockKind::Data)
            .map(|_| ())
    }

    /// Commit one inode: its data blocks, the allocation state that makes them
    /// reachable, and its on-disk record.
    ///
    /// Two things are deliberately outside the scope. The directory entry
    /// naming the inode is not committed: POSIX says an `fsync` on the parent
    /// directory does that, and a crash before the entry lands leaves an
    /// orphan inode, not a corrupt file. Nor is the superblock's free-count
    /// drift, which `e2fsck` recomputes and which a crash already mandates a
    /// check for (the mount stamped `EXT2_ERROR_FS`).
    ///
    /// Ordering follows `data=ordered` narrowed to one inode: data and
    /// allocation blocks, a barrier, then the inode-table block. A crash
    /// between the two leaves a record whose size predates the data, never one
    /// whose blocks hold a previous file's contents. Allocation state goes in
    /// the *first* phase because an inode published while the bitmap still
    /// calls its blocks free is an invitation to hand them to a second file.
    ///
    /// `data_only` currently selects nothing: an ext2 record carries the block
    /// pointers, the size and the timestamps in one 128-byte struct, so no
    /// write commits the first two without the third. It divides only once a
    /// timestamp alone can dirty a record, which needs the wall clock.
    pub fn sync_inode(&mut self, ino: u32, data_only: bool) -> Result<(), Ext2Error> {
        let _ = data_only;
        let ino_num = InodeNum(ino);
        // Proves the number lands inside a group's inode table before anything
        // is written on its behalf.
        let (table_block, _) = self.inode_disk_offset(ino_num)?;

        let (first, last) = self.inode_table_span(ino_num, table_block)?;

        // The record shares its block with its neighbours, so writing it
        // publishes their cached state too. Their data goes out in the same
        // phase as this inode's, or the barrier below would order a metadata
        // write ahead of data it already names.
        self.cache
            .flush_where(self.device, |_, owner| match owner {
                BlockOwner::File(owned) => owned >= first && owned <= last,
                BlockOwner::Alloc => true,
                BlockOwner::Inodes { .. } | BlockOwner::Other => false,
            })?;

        // Eviction may already have written this inode's data with no barrier
        // behind it, so what must be ordered is the device's commits, not this
        // function's writes.
        if self.cache.unbarriered_writes() > 0 {
            self.device_barrier()?;
        }
        self.cache.flush_block(table_block, self.device)?;
        if self.cache.unbarriered_writes() == 0 {
            return Ok(());
        }
        self.device_barrier()
    }

    /// Inclusive range of inode numbers whose records share `table_block`.
    ///
    /// Widened by one record on each side when `inode_size` does not divide
    /// the block size, because then a record straddles the boundary. Erring
    /// wide costs a few extra data blocks in the pre-flush; erring narrow
    /// would publish a neighbour's record ahead of its data.
    fn inode_table_span(
        &mut self,
        ino: InodeNum,
        table_block: BlockNum,
    ) -> Result<(u32, u32), Ext2Error> {
        let (group, _) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let table_start = self
            .read_group_desc(group)?
            .inode_table
            .to_disk_offset(self.block_size)
            .raw();
        let block_start = table_block.to_disk_offset(self.block_size).raw();
        let into_table = block_start
            .checked_sub(table_start)
            .ok_or(Ext2Error::InvalidInode)?;

        let inode_size = self.inode_size.max(1) as u64;
        let per_block = (self.block_size as u64).div_ceil(inode_size);
        let first_local = (into_table / inode_size) as u32;
        let aligned = self.block_size as u64 % inode_size == 0;
        let slack = u32::from(!aligned);

        // Saturating throughout: the inputs are superblock-derived, so a
        // hostile image must widen the span (which only costs a larger
        // pre-flush) rather than overflow.
        let per_group = self.geom.inodes_per_group();
        let base = group
            .raw()
            .saturating_mul(per_group)
            .saturating_add(1)
            .min(self.geom.inodes_count());
        let group_last = base
            .saturating_add(per_group.saturating_sub(1))
            .min(self.geom.inodes_count());
        let first_in_block = base.saturating_add(first_local);
        let first = first_in_block.saturating_sub(slack).max(base);
        let last = first_in_block
            .saturating_add(per_block as u32)
            .saturating_sub(1)
            .saturating_add(slack)
            .min(group_last);
        Ok((first, last.max(first)))
    }

    /// Whether a writeback pass would do anything at all.
    ///
    /// A barrier over nothing orders nothing, and this is asked on every
    /// flusher tick and every `sync(2)`. The log counts as work even with a
    /// clean cache: a rollback can leave a block whose only copy is a record.
    pub fn sync_pending(&self) -> bool {
        self.cache.dirty_count() > 0
            || self.cache.unbarriered_writes() > 0
            || self.superblock_dirty
            || !self.cache.journal_is_empty()
    }

    /// Open a writeback pass over everything dirty *now*.
    ///
    /// The epoch is fixed here, so a pass driven one [`Self::sync_step`] at a
    /// time with the mount lock dropped in between still writes nothing a
    /// later operation dirtied — which is what keeps the phases ordered.
    pub fn begin_sync(&self) -> SyncPass {
        SyncPass {
            epoch: self.cache.writeback_epoch(),
            phase: SyncPhase::Data,
            cursor: 1,
            limit: self.cache.journal_head(),
            generation: self.cache.journal_generation(),
        }
    }

    /// Advance `pass` by at most `budget` device writes.
    ///
    /// Ordered durability, following ext2's `data=ordered` discipline: data
    /// blocks, barrier, metadata blocks, the log's own leftovers, barrier,
    /// superblock free counts, barrier. A crash between phases can leave
    /// recoverable free-count drift but never a directory entry or inode
    /// pointing at uninitialised on-disk data.
    pub fn sync_step(&mut self, pass: &mut SyncPass, budget: usize) -> Result<(), Ext2Error> {
        match pass.phase {
            SyncPhase::Data => {
                let progress =
                    self.cache
                        .flush_bounded(self.device, pass.epoch, budget, |kind, _| {
                            kind == cache::BlockKind::Data
                        })?;
                if !progress.more {
                    self.device_barrier()?;
                    pass.phase = SyncPhase::Metadata;
                }
            }
            SyncPhase::Metadata => {
                let progress =
                    self.cache
                        .flush_bounded(self.device, pass.epoch, budget, |kind, _| {
                            kind == cache::BlockKind::Metadata
                        })?;
                if !progress.more {
                    pass.phase = SyncPhase::Logged;
                }
            }
            SyncPhase::Logged => {
                // A log emptied behind the pass invalidates both indices, and
                // whoever reset it check pointed those records — so the rest
                // is the next pass's.
                if self.cache.journal_generation() != pass.generation {
                    pass.phase = SyncPhase::Superblock;
                    return Ok(());
                }
                // Bounded by the head the pass opened on: a record appended
                // since may belong to an operation still running.
                let progress =
                    self.cache
                        .checkpoint_logged(self.device, pass.cursor, budget, pass.limit)?;
                pass.cursor = progress.cursor;
                if !progress.more {
                    if self.cache.unbarriered_writes() > 0 {
                        self.device_barrier()?;
                    }
                    // Only when nothing was appended behind the pass: half an
                    // emptied log is not a state the format can express.
                    if self.cache.journal_head() == pass.limit {
                        self.cache.journal_reset(self.device)?;
                    }
                    pass.phase = SyncPhase::Superblock;
                }
            }
            SyncPhase::Superblock => {
                if self.superblock_dirty {
                    self.write_superblock()?;
                    self.device_barrier()?;
                    self.superblock_dirty = false;
                }
                pass.phase = SyncPhase::Done;
            }
            SyncPhase::Done => {}
        }
        Ok(())
    }

    /// Drive a whole pass without releasing anything. The callers that can
    /// afford to yield use [`Self::sync_step`].
    pub fn sync(&mut self) -> Result<(), Ext2Error> {
        if !self.sync_pending() {
            return Ok(());
        }
        let mut pass = self.begin_sync();
        while !pass.is_done() {
            self.sync_step(&mut pass, usize::MAX)?;
        }
        Ok(())
    }

    /// Attach the image's metadata log, replaying whatever a previous boot
    /// committed and never check pointed.
    ///
    /// `Ok(None)` for an image with no usable log: a journal is an ext2
    /// image's optional property, and without one operations fall back to
    /// undo-scoped. A read-only mount attaches none — replay is a write.
    #[inline(never)]
    pub fn attach_journal(&mut self) -> Result<Option<journal::JournalRecovery>, Ext2Error> {
        let ino = match self.resolve_path(JOURNAL_PATH) {
            Ok(ino) => ino,
            Err(Ext2Error::PathNotFound | Ext2Error::NotDirectory | Ext2Error::NotFile) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let inode = self.read_inode_num(InodeNum(ino))?;
        // The seal is what refuses every write, rename and unlink of the file,
        // so a log file without one is not a log this kernel will use.
        if !inode.is_regular_file() || !inode.is_immutable() {
            return Ok(None);
        }
        let blocks = u32::try_from(inode.size / self.block_size as u64).unwrap_or(0);
        if blocks < journal::MIN_LOG_SLOTS + 1 {
            return Ok(None);
        }
        // Recorded once the shape says it really is a log, and before the
        // read-only bail: a mount that cannot replay still owes readers the
        // refusal. Earlier would reserve the name against an ordinary file,
        // which *can* be deleted — and the refusal would then follow its inode
        // number to whatever reused it.
        self.journal_inode = Some(ino);
        if self.read_only {
            return Ok(None);
        }
        let Some(slots) = self.map_log_blocks(ino, blocks)? else {
            return Ok(None);
        };
        let extent = journal::LogExtent {
            first_data_block: self.geom.first_data_block().raw(),
            blocks_count: self.geom.blocks_count(),
        };
        let (log, recovery) =
            journal::Journal::attach(slots, self.block_size, ino, extent, self.device)?;
        if recovery.replayed() {
            // The replay wrote home locations under the reads this function
            // just did, so anything cached before it is stale.
            self.cache.invalidate_all_clean();
            // Including the blocks that named the log's own extent: if the
            // replay moved them, this list is no longer `/.journal`'s.
            if !self.log_blocks_unchanged(ino, blocks, log.slots())? {
                return Ok(None);
            }
        }
        self.cache.install_journal(log)?;
        Ok(Some(recovery))
    }

    /// Whether the log file still maps to exactly `slots`.
    #[inline(never)]
    fn log_blocks_unchanged(
        &mut self,
        ino: u32,
        blocks: u32,
        slots: &[u32],
    ) -> Result<bool, Ext2Error> {
        let again = self.map_log_blocks(ino, blocks)?;
        Ok(again.as_ref().map(|s| s.as_slice()) == Some(slots))
    }

    /// The log file's blocks, or `None` for a file this kernel will not log
    /// into. Each is checked against the volume, because the log writes to
    /// them without consulting the filesystem again.
    fn map_log_blocks(&mut self, ino: u32, blocks: u32) -> Result<Option<KVec<u32>>, Ext2Error> {
        let inode = self.read_inode_num(InodeNum(ino))?;
        let mut slots = KVec::with_capacity(blocks as usize).map_err(|_| Ext2Error::OutOfMemory)?;
        for index in 0..blocks {
            let block = blockmap::map_block(
                &inode,
                FileBlock(index),
                self.ptrs_per_block,
                &mut *self.cache,
                self.device,
                BlockOwner::File(ino),
            )?;
            // A hole means the file was never preallocated; a block outside
            // the volume means the mapping is not this file's. Writing into
            // either would land on something else.
            if !block.is_valid() || self.geom.checked_block(block.raw()).is_none() {
                return Ok(None);
            }
            slots
                .push(block.raw())
                .map_err(|_| Ext2Error::OutOfMemory)?;
        }
        Ok(Some(slots))
    }

    /// Whether the log has filled far enough that the flusher should drain it
    /// before an operation is forced to check point under the mount lock.
    pub fn journal_needs_drain(&self) -> bool {
        self.cache.journal_needs_drain()
    }

    /// Where the log's append point stands. `1` is empty, and so is no log.
    pub fn journal_head(&self) -> u32 {
        self.cache.journal_head()
    }

    /// Empty the log so the next operation has room to log itself.
    fn checkpoint_journal(&mut self) -> Result<(), Ext2Error> {
        self.sync()
    }

    pub fn flush(&mut self) -> Result<(), Ext2Error> {
        self.sync()
    }

    fn device_barrier(&mut self) -> Result<(), Ext2Error> {
        self.device.flush().map_err(|_| Ext2Error::DeviceError)?;
        self.cache.note_barrier();
        Ok(())
    }

    pub fn read_inode(&mut self, ino: u32) -> Result<Inode, Ext2Error> {
        self.read_inode_num(InodeNum(ino))
    }

    /// Byte offset of an inode inside the image, proven to lie within the
    /// group's inode table rather than derived from an unchecked descriptor.
    fn inode_disk_offset(&mut self, ino: InodeNum) -> Result<(BlockNum, usize), Ext2Error> {
        let (group, local) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let desc = self.read_group_desc(group)?;
        if !desc.inode_table.is_valid() {
            return Err(Ext2Error::InvalidInode);
        }
        let byte_off = desc.inode_table.to_disk_offset(self.block_size).raw()
            + (local as u64 * self.inode_size as u64);
        let blk_raw = u32::try_from(byte_off / self.block_size as u64)
            .map_err(|_| Ext2Error::InvalidInode)?;
        let blk_num = self
            .geom
            .checked_block(blk_raw)
            .ok_or(Ext2Error::InvalidInode)?;
        let within = (byte_off % self.block_size as u64) as usize;
        if within + self.inode_size as usize > self.block_size as usize {
            return Err(Ext2Error::InvalidInode);
        }
        Ok((blk_num, within))
    }

    fn read_inode_num(&mut self, ino: InodeNum) -> Result<Inode, Ext2Error> {
        let (blk_num, within) = self.inode_disk_offset(ino)?;
        let size = self.inode_size as usize;
        let owner = self.inode_block_owner(ino, blk_num)?;
        let block = self.cache.get_owned(blk_num, self.device, owner)?;
        let data = block.data();
        if within + size > data.len() {
            return Err(Ext2Error::InvalidInode);
        }
        Ok(Inode::parse(&data[within..within + size]))
    }

    /// Classify an inode-table block by the records it carries, so a per-inode
    /// sync can tell it apart from a directory block. A span that fails to
    /// close degrades to [`BlockOwner::Other`], which is the conservative
    /// answer: `sync_inode` then declines to publish it early.
    fn inode_block_owner(
        &mut self,
        ino: InodeNum,
        table_block: BlockNum,
    ) -> Result<BlockOwner, Ext2Error> {
        let (first, last) = self.inode_table_span(ino, table_block)?;
        Ok(BlockOwner::Inodes { first, last })
    }

    fn write_inode_num(&mut self, ino: InodeNum, inode: &Inode) -> Result<(), Ext2Error> {
        let (blk_num, within) = self.inode_disk_offset(ino)?;
        let size = self.inode_size as usize;
        let owner = self.inode_block_owner(ino, blk_num)?;
        let mut block = self.cache.get_owned(blk_num, self.device, owner)?;
        let data = block.data_mut();
        if within + size > data.len() {
            return Err(Ext2Error::InvalidInode);
        }
        inode.encode(&mut data[within..within + size]);
        Ok(())
    }

    /// The log's own file is refused rather than read: its blocks hold copies
    /// of bitmaps, inode tables and directory blocks, so a reader of it sees
    /// the metadata of files whose permissions it does not hold.
    /// `EXT2_IMMUTABLE_FL` refuses the write half; this is the read half.
    pub fn read_file(
        &mut self,
        ino: u32,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        if self.journal_inode == Some(ino) {
            return Err(Ext2Error::Immutable);
        }
        let inode = self.read_inode_num(InodeNum(ino))?;
        file::read_file(
            &inode,
            offset,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
        )
    }

    #[inline(never)]
    pub fn write_file(&mut self, ino: u32, offset: u64, buffer: &[u8]) -> Result<usize, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            let free_before = fs.superblock.free_blocks_count;
            // A short write still updated the size and kept its blocks, so the
            // inode record must be written whichever way this went; only a
            // zero-byte failure propagates as an error.
            let result = file::write_file(
                &mut inode,
                offset,
                buffer,
                &mut *fs.cache,
                fs.device,
                fs.ptrs_per_block,
                fs.block_size,
                &mut fs.superblock,
                &fs.geom,
                BlockOwner::File(ino),
            );
            let written = result?;
            time::stamp(&mut inode.mtime);
            time::stamp(&mut inode.ctime);
            fs.note_large_file(&inode);
            fs.write_inode_num(ino_num, &inode)?;
            if fs.superblock.free_blocks_count != free_before {
                fs.superblock_dirty = true;
            }
            Ok(written)
        })
    }

    /// Shrink or extend a regular file. Extension is sparse, following ext2:
    /// the hole reads as zeros and costs no blocks until it is written.
    #[inline(never)]
    pub fn truncate_file(&mut self, ino: u32, new_size: u64) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            if inode.is_directory() {
                return Err(Ext2Error::IsDirectory);
            }
            if !inode.is_regular_file() {
                return Err(Ext2Error::NotFile);
            }
            // A size past what the block map can address would leave `i_size`
            // naming a block no read could ever reach, and every later read of
            // the hole would fail. Refused here rather than at that read: the
            // size is the caller's, and this is the only place it is set.
            if new_size > blockmap::max_file_size(fs.ptrs_per_block, fs.block_size) {
                return Err(Ext2Error::InvalidRange);
            }
            let free_before = fs.superblock.free_blocks_count;
            fs.release_blocks_past(&mut inode, new_size, BlockOwner::File(ino))?;
            time::stamp(&mut inode.mtime);
            time::stamp(&mut inode.ctime);
            fs.note_large_file(&inode);
            fs.write_inode_num(ino_num, &inode)?;
            if fs.superblock.free_blocks_count != free_before {
                fs.superblock_dirty = true;
            }
            fs.superblock_dirty = true;
            Ok(())
        })
    }

    /// A file above 4 GiB is only correctly read by an implementation that
    /// knows to consult `i_size_high`, and ext2 spells that as a
    /// read-only-compatible feature bit. Writing one without setting the bit
    /// hands every other reader a truncated size.
    fn note_large_file(&mut self, inode: &Inode) {
        if inode.needs_large_file_feature()
            && self.superblock.feature_ro_compat & RO_COMPAT_LARGE_FILE == 0
        {
            self.superblock.feature_ro_compat |= RO_COMPAT_LARGE_FILE;
            self.superblock_dirty = true;
        }
    }

    /// Free every block past `new_size` and update the inode's size and block
    /// count. Shared by `truncate_file` and directory removal.
    fn release_blocks_past(
        &mut self,
        inode: &mut Inode,
        new_size: u64,
        owner: BlockOwner,
    ) -> Result<(), Ext2Error> {
        let geom = self.geom;
        let superblock = &mut self.superblock;
        let cache = &mut *self.cache;
        let device = self.device;
        // `free_block` needs the same cache and superblock the walk borrows,
        // so the frees are collected and applied after it.
        let mut freed: slopos_ostd::KVec<BlockNum> = slopos_ostd::KVec::new();
        file::truncate(
            inode,
            new_size,
            cache,
            device,
            self.ptrs_per_block,
            self.block_size,
            owner,
            &mut |b| freed.push(b).map_err(|_| Ext2Error::OutOfMemory),
        )?;
        for blk in freed.iter() {
            ext2_alloc::free_block(*blk, &geom, superblock, cache, device)?;
        }
        Ok(())
    }

    /// Read a symlink's target into `buffer`, answering the byte count.
    #[inline(never)]
    pub fn read_symlink(&mut self, ino: u32, buffer: &mut [u8]) -> Result<usize, Ext2Error> {
        let inode = self.read_inode_num(InodeNum(ino))?;
        symlink::read_symlink(
            &inode,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
        )
    }

    /// Write `mode`'s permission bits through to `i_mode`, leaving the type
    /// nibble — a caller must not turn a file into a directory.
    #[inline(never)]
    pub fn set_mode(&mut self, ino: u32, mode: u16) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            inode.mode = (inode.mode & !MODE_PERM_MASK) | (mode & MODE_PERM_MASK);
            time::stamp(&mut inode.ctime);
            fs.write_inode_num(ino_num, &inode)
        })
    }

    /// Seal an inode with `EXT2_IMMUTABLE_FL`. One-way, as the VFS trait
    /// requires: nothing in this implementation clears the bit.
    #[inline(never)]
    pub fn set_sealed(&mut self, ino: u32) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Ok(());
            }
            inode.flags |= EXT2_IMMUTABLE_FL;
            time::stamp(&mut inode.ctime);
            fs.write_inode_num(ino_num, &inode)
        })
    }

    pub fn is_sealed(&mut self, ino: u32) -> Result<bool, Ext2Error> {
        Ok(self.read_inode_num(InodeNum(ino))?.is_immutable())
    }

    pub fn for_each_dir_entry<F>(&mut self, ino: u32, mut f: F) -> Result<(), Ext2Error>
    where
        F: FnMut(DirEntry<'_>) -> bool,
    {
        let inode = self.read_inode_num(InodeNum(ino))?;
        dir::for_each_entry(
            &inode,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
            &mut f,
        )
    }

    /// Walk a directory from a byte-offset cookie, handing the callback the
    /// cookie that resumes *after* each entry. Answers the cookie the walk
    /// reached, which equals the directory's size once it is exhausted.
    pub fn for_each_dir_entry_from<F>(
        &mut self,
        ino: u32,
        start: u64,
        mut f: F,
    ) -> Result<u64, Ext2Error>
    where
        F: FnMut(u64, DirEntry<'_>) -> bool,
    {
        let inode = self.read_inode_num(InodeNum(ino))?;
        dir::for_each_entry_from(
            &inode,
            start,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
            &mut f,
        )
    }

    pub fn resolve_path(&mut self, path: &[u8]) -> Result<u32, Ext2Error> {
        if path.is_empty() || path[0] != b'/' {
            return Err(Ext2Error::PathNotFound);
        }
        let mut current = InodeNum::ROOT;
        for component in path[1..].split(|&b| b == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            let inode = self.read_inode_num(current)?;
            if !inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            if component == b".." {
                let mut parent = None;
                dir::for_each_entry(
                    &inode,
                    &mut *self.cache,
                    self.device,
                    self.ptrs_per_block,
                    self.block_size,
                    BlockOwner::File(current.raw()),
                    &mut |e| {
                        if e.name == b".." {
                            parent = Some(e.inode);
                            false
                        } else {
                            true
                        }
                    },
                )?;
                current = parent.unwrap_or(current);
            } else {
                current = dir::lookup_child(
                    &inode,
                    component,
                    &mut *self.cache,
                    self.device,
                    self.ptrs_per_block,
                    self.block_size,
                    BlockOwner::File(current.raw()),
                )?;
            }
        }
        Ok(current.raw())
    }

    #[inline(never)]
    pub fn create_file(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.create_inode_entry(InodeNum(parent), name, NewInode::File))
            .map(|n| n.raw())
    }

    #[inline(never)]
    pub fn create_directory(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.create_inode_entry(InodeNum(parent), name, NewInode::Directory))
            .map(|n| n.raw())
    }

    /// Create a symlink under `parent` pointing at `target`. Targets of 60
    /// bytes or fewer are fast symlinks, stored inline in `i_block`.
    pub fn create_symlink(
        &mut self,
        parent: u32,
        name: &[u8],
        target: &[u8],
    ) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.create_inode_entry(InodeNum(parent), name, NewInode::Symlink(target))
        })
        .map(|n| n.raw())
    }

    #[inline(never)]
    fn create_inode_entry(
        &mut self,
        parent_num: InodeNum,
        name: &[u8],
        kind: NewInode<'_>,
    ) -> Result<InodeNum, Ext2Error> {
        if name.is_empty() || name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        if name == b"." || name == b".." {
            return Err(Ext2Error::AlreadyExists);
        }
        let mut parent = self.read_inode_num(parent_num)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent.is_immutable() {
            return Err(Ext2Error::Immutable);
        }
        let is_dir = matches!(kind, NewInode::Directory);
        // A directory's parent gains a `..` link, and `links_count` is a u16.
        if is_dir && parent.links_count == u16::MAX {
            return Err(Ext2Error::TooManyLinks);
        }

        // Without this a second create writes a second record under the same
        // name: lookup answers whichever comes first and the other inode is
        // unreachable, leaving the image inconsistent for every other ext2
        // implementation.
        if dir::lookup_child(
            &parent,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent_num.raw()),
        )
        .is_ok()
        {
            return Err(Ext2Error::AlreadyExists);
        }

        let parent_group = self
            .geom
            .locate_inode(parent_num)
            .map(|(g, _)| g)
            .ok_or(Ext2Error::InvalidInode)?;
        let new_ino = ext2_alloc::allocate_inode(
            parent_group,
            &self.geom,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
        )?;

        let mut new_inode = self.build_new_inode(new_ino, parent_num, kind)?;
        let now = time::now_unix();
        new_inode.atime = now;
        new_inode.ctime = now;
        new_inode.mtime = now;
        self.write_inode_num(new_ino, &new_inode)?;

        let ft = match kind {
            NewInode::Directory => DIR_FT_DIR,
            NewInode::File => DIR_FT_REG_FILE,
            NewInode::Symlink(_) => DIR_FT_SYMLINK,
        };
        dir::append_dir_entry(
            &mut parent,
            new_ino,
            name,
            ft,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut self.superblock,
            &self.geom,
            BlockOwner::File(parent_num.raw()),
        )?;

        if is_dir {
            parent.links_count += 1;
            self.adjust_used_dirs(new_ino, 1)?;
        }
        time::stamp(&mut parent.mtime);
        time::stamp(&mut parent.ctime);
        self.write_inode_num(parent_num, &parent)?;
        self.superblock_dirty = true;

        Ok(new_ino)
    }

    /// The in-memory record for a newly allocated inode, with the data blocks
    /// a directory or a slow symlink needs already written.
    #[inline(never)]
    fn build_new_inode(
        &mut self,
        new_ino: InodeNum,
        parent_num: InodeNum,
        kind: NewInode<'_>,
    ) -> Result<Inode, Ext2Error> {
        if let NewInode::Symlink(target) = kind {
            let data_block = if symlink::symlink_needs_block(target) {
                Some(ext2_alloc::allocate_block(
                    &self.geom,
                    &mut self.superblock,
                    &mut *self.cache,
                    self.device,
                )?)
            } else {
                None
            };
            return symlink::create_symlink_inode(
                target,
                self.block_size,
                data_block,
                &mut *self.cache,
                self.device,
                BlockOwner::File(new_ino.raw()),
            );
        }

        let is_dir = matches!(kind, NewInode::Directory);
        let mut inode = Inode {
            mode: if is_dir {
                MODE_DIRECTORY | 0o755
            } else {
                MODE_FILE | 0o644
            },
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: if is_dir { 2 } else { 1 },
            blocks: 0,
            flags: 0,
            block: [BlockNum::ZERO; 15],
        };
        if !is_dir {
            return Ok(inode);
        }

        let first_block = ext2_alloc::allocate_block(
            &self.geom,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
        )?;
        {
            let mut blk = self.cache.get_zero_data(
                first_block,
                self.device,
                BlockOwner::File(new_ino.raw()),
            )?;
            let data = blk.data_mut();
            let bs = self.block_size as usize;
            let dot_rec = 12;
            ondisk::write_dir_entry(&mut data[..dot_rec], new_ino, b".", DIR_FT_DIR, dot_rec);
            let dotdot_rec = bs - dot_rec;
            ondisk::write_dir_entry(
                &mut data[dot_rec..dot_rec + dotdot_rec],
                parent_num,
                b"..",
                DIR_FT_DIR,
                dotdot_rec,
            );
        }
        inode.block[0] = first_block;
        inode.size = self.block_size as u64;
        inode.blocks = self.block_size / 512;
        Ok(inode)
    }

    /// Move the `used_dirs_count` of the group holding `ino` by `delta`.
    /// `e2fsck` reports a stale one, and the allocator's directory-spreading
    /// heuristic is what the field exists for.
    fn adjust_used_dirs(&mut self, ino: InodeNum, delta: i32) -> Result<(), Ext2Error> {
        let (group, _) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let mut desc = self.read_group_desc(group)?;
        desc.used_dirs_count = if delta >= 0 {
            desc.used_dirs_count.saturating_add(delta as u16)
        } else {
            desc.used_dirs_count.saturating_sub((-delta) as u16)
        };
        ext2_alloc::write_group_desc(group, &desc, &self.geom, &mut *self.cache, self.device)
    }

    /// Remove a name, freeing the inode when it was the last one.
    #[inline(never)]
    pub fn unlink_entry(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.remove_entry(
                InodeNum(parent),
                name,
                RemoveKind::NonDirectory,
                LastLink::Free,
            )
            .map(|_| ())
        })
    }

    /// Remove a name, orphaning the inode rather than freeing it when it was
    /// the last one. Answers the orphaned inode, which the caller owes a
    /// [`Self::release_orphan`] once nothing holds it open.
    #[inline(never)]
    pub fn detach_entry(&mut self, parent: u32, name: &[u8]) -> Result<Option<u32>, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.remove_entry(
                InodeNum(parent),
                name,
                RemoveKind::NonDirectory,
                LastLink::Orphan,
            )
            .map(|o| o.map(|n| n.raw()))
        })
    }

    /// Remove an empty directory: `rmdir(2)`'s semantics, including the
    /// parent's `links_count` decrement for the vanished `..`.
    ///
    /// Never orphans: `open` refuses a directory, so no descriptor can be
    /// holding one when its last link goes.
    #[inline(never)]
    pub fn remove_directory(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.remove_entry(
                InodeNum(parent),
                name,
                RemoveKind::Directory,
                LastLink::Free,
            )
            .map(|_| ())
        })
    }

    /// Answers the inode that was orphaned rather than freed, if any.
    #[inline(never)]
    fn remove_entry(
        &mut self,
        parent_num: InodeNum,
        name: &[u8],
        kind: RemoveKind,
        last_link: LastLink,
    ) -> Result<Option<InodeNum>, Ext2Error> {
        if name == b"." || name == b".." {
            return Err(Ext2Error::NotEmpty);
        }
        let mut parent_inode = self.read_inode_num(parent_num)?;
        if !parent_inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent_inode.is_immutable() {
            return Err(Ext2Error::Immutable);
        }

        let target_num = dir::lookup_child(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent_num.raw()),
        )?;
        let mut target = self.read_inode_num(target_num)?;
        if target.is_immutable() {
            return Err(Ext2Error::Immutable);
        }

        let is_dir = target.is_directory();
        match kind {
            RemoveKind::NonDirectory if is_dir => return Err(Ext2Error::IsDirectory),
            RemoveKind::Directory if !is_dir => return Err(Ext2Error::NotDirectory),
            _ => {}
        }
        if is_dir
            && !dir::is_dir_empty(
                &target,
                &mut *self.cache,
                self.device,
                self.ptrs_per_block,
                self.block_size,
                BlockOwner::File(target_num.raw()),
            )?
        {
            return Err(Ext2Error::NotEmpty);
        }

        dir::remove_dir_entry(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent_num.raw()),
        )?;

        // A name is not the inode. An image `mkfs` or any other kernel wrote
        // may carry several names for one inode, and freeing its blocks while
        // another name still points at them hands a live file's contents to
        // the next allocation. A directory is exempt: its two links are `.`
        // and its parent's entry, both of which this removal takes with it.
        let was_last = is_dir || target.links_count <= 1;
        let mut orphaned = None;
        if !was_last {
            target.links_count -= 1;
            time::stamp(&mut target.ctime);
            self.write_inode_num(target_num, &target)?;
        } else if last_link == LastLink::Orphan {
            // POSIX: the name is gone but the file is not, because a
            // descriptor still refers to it. `links_count` drops to zero so no
            // other reader treats the inode as reachable, and the orphan list
            // is what tells the next `e2fsck` to finish the free if this boot
            // never gets to.
            target.links_count = 0;
            time::stamp(&mut target.ctime);
            self.write_inode_num(target_num, &target)?;
            self.orphan_push(target_num)?;
            orphaned = Some(target_num);
        } else {
            self.free_detached_inode(target_num, &mut target, is_dir)?;
        }

        // Re-read the parent: `remove_dir_entry` mutated its cached data
        // block, and `append_dir_entry` in an earlier op may have grown it.
        parent_inode = self.read_inode_num(parent_num)?;
        if is_dir {
            parent_inode.links_count = parent_inode.links_count.saturating_sub(1);
        }
        time::stamp(&mut parent_inode.mtime);
        time::stamp(&mut parent_inode.ctime);
        self.write_inode_num(parent_num, &parent_inode)?;
        self.superblock_dirty = true;

        Ok(orphaned)
    }

    /// Free the blocks and the inode of a record that no name reaches.
    #[inline(never)]
    fn free_detached_inode(
        &mut self,
        target_num: InodeNum,
        target: &mut Inode,
        is_dir: bool,
    ) -> Result<(), Ext2Error> {
        // A fast symlink's target lives in `i_block`, which the block walk
        // would otherwise read as fifteen block numbers and free.
        if !target.is_fast_symlink() {
            self.release_file_blocks(target, BlockOwner::File(target_num.raw()))?;
        }
        ext2_alloc::free_inode(
            target_num,
            &self.geom,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
        )?;
        if is_dir {
            self.adjust_used_dirs(target_num, -1)?;
        }

        // `i_dtime` is what every other ext2 implementation stamps on a free,
        // and what `e2fsck` reads to tell a freed inode from a corrupt one.
        target.mode = 0;
        target.links_count = 0;
        target.blocks = 0;
        target.size = 0;
        target.flags = 0;
        target.block = [BlockNum::ZERO; 15];
        target.dtime = time::now_unix();
        self.write_inode_num(target_num, target)
    }

    /// Complete the deferred free of an orphaned inode: unthread it from the
    /// list and release its blocks.
    ///
    /// Idempotent against an inode that is not on the list — a second call, or
    /// a call for an inode a crashing boot's `e2fsck` already drained, does
    /// nothing rather than freeing a live file.
    #[inline(never)]
    pub fn release_orphan(&mut self, ino: u32) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino)?;
            // A record that gained a link, or that is already freed, is not
            // this orphan any more. Freeing it would take a live file with it.
            if inode.links_count != 0 || inode.mode == 0 {
                return Ok(());
            }
            if !fs.orphan_remove(ino)? {
                return Ok(());
            }
            let is_dir = inode.is_directory();
            fs.free_detached_inode(ino, &mut inode, is_dir)?;
            fs.superblock_dirty = true;
            Ok(())
        })
    }

    /// Push `ino` onto the head of the on-disk orphan list.
    ///
    /// The inode's own record carries the next member's number in `i_dtime`,
    /// which is ext2's own mechanism: a freed inode's `i_dtime` is a deletion
    /// timestamp, and an orphan's is a link, told apart by `links_count == 0`
    /// with a nonzero `i_mode`. The head is written last, so a crash between
    /// the two leaves a list that is shorter than the truth — leaked blocks
    /// `e2fsck` reclaims — rather than one naming an inode that never joined.
    fn orphan_push(&mut self, ino: InodeNum) -> Result<(), Ext2Error> {
        let head = self.superblock.last_orphan;
        if head == ino.raw() {
            return Ok(());
        }
        let mut inode = self.read_inode_num(ino)?;
        inode.dtime = head;
        self.write_inode_num(ino, &inode)?;
        // The member's next-pointer must be *on the device* before the head
        // names it. `write_inode_num` only dirties a cached block, whereas
        // `write_orphan_head` writes through and barriers, so without this the
        // real on-disk order is the inverse: a crash would leave the head
        // naming an inode whose `i_dtime` is still its pre-push value — zero
        // on a freshly created one — truncating the whole chain behind it and
        // leaking every orphan already on the list.
        // A home write of an uncommitted record is what the log forbids;
        // under one the ordering comes from the commit, because
        // `write_orphan_head` defers past it.
        if self.cache.journal().is_none() {
            self.flush_inode_record(ino)?;
        }
        self.superblock.last_orphan = ino.raw();
        self.superblock_dirty = true;
        self.write_orphan_head()
    }

    /// Put one inode's table block on the device and barrier behind it.
    ///
    /// Narrower than [`Self::sync_inode`] on purpose: this orders one record
    /// against a superblock field, and pulling the inode's data blocks along
    /// would make an `unlink` pay for a writeback it does not need.
    fn flush_inode_record(&mut self, ino: InodeNum) -> Result<(), Ext2Error> {
        let (table_block, _) = self.inode_disk_offset(ino)?;
        if self.cache.flush_block(table_block, self.device)? {
            self.device_barrier()?;
        }
        Ok(())
    }

    /// Unthread `ino`, answering whether it was on the list at all.
    ///
    /// Bounded by the inode count: a damaged image can carry a cycle, and this
    /// walk must terminate rather than spin under the mount lock.
    fn orphan_remove(&mut self, ino: InodeNum) -> Result<bool, Ext2Error> {
        let target = ino.raw();
        if self.superblock.last_orphan == target {
            let next = self.read_inode_num(ino)?.dtime;
            self.superblock.last_orphan = next;
            self.superblock_dirty = true;
            // Immediate, where a push defers: removal shares a transaction
            // with the free that overwrites this member's `i_dtime`, so a head
            // published after the commit would name an inode whose chain link
            // is already gone, and the next mount's drain would stop there and
            // discard everything behind it.
            self.write_orphan_head_now()?;
            return Ok(true);
        }

        let mut current = self.superblock.last_orphan;
        let mut steps = 0u32;
        let limit = self.geom.inodes_count();
        while current != 0 {
            steps += 1;
            if steps > limit {
                return Err(Ext2Error::DirectoryFormat);
            }
            let mut prev = self.read_inode_num(InodeNum(current))?;
            if prev.dtime == target {
                prev.dtime = self.read_inode_num(ino)?.dtime;
                self.write_inode_num(InodeNum(current), &prev)?;
                return Ok(true);
            }
            current = prev.dtime;
        }
        Ok(false)
    }

    /// The head pointer goes to the device on the spot, barriered, rather than
    /// waiting for a `sync`.
    ///
    /// This is the field that makes an unreachable inode recoverable, and the
    /// window it covers is exactly the one in which the kernel might not get
    /// to write anything again. It is a sub-block write, invisible to the
    /// cache, so it costs nothing an ordinary operation was going to pay.
    ///
    /// Under a log it is *deferred to the commit*: the ordering the list needs
    /// is then the log's, and writing the head from inside the operation would
    /// publish a field of a transaction that may still roll back.
    fn write_orphan_head(&mut self) -> Result<(), Ext2Error> {
        if self.in_transaction && self.cache.journal().is_some() {
            self.pending_orphan_head = Some(self.superblock.last_orphan);
            return Ok(());
        }
        self.write_orphan_head_now()
    }

    /// Whether a deferred head write is owed.
    fn take_pending_orphan_head(&mut self) -> Option<u32> {
        self.pending_orphan_head.take()
    }

    #[inline(never)]
    fn write_orphan_head_now(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        sb_buf[S_LAST_ORPHAN_OFF..S_LAST_ORPHAN_OFF + 4]
            .copy_from_slice(&self.superblock.last_orphan.to_le_bytes());
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.device_barrier()
    }

    /// Free every inode the orphan list names, at mount, on an image this
    /// implementation may write. Answers how many were reclaimed.
    ///
    /// This is the crash-recovery half: a boot that died with an unlinked file
    /// still open left its blocks allocated and reachable from nowhere but
    /// this list. Draining is bounded by the inode count and stops at the
    /// first member that no longer looks like an orphan, so a damaged list
    /// leaks space rather than freeing a live file.
    #[inline(never)]
    pub fn drain_orphans(&mut self) -> Result<u32, Ext2Error> {
        if self.read_only || self.superblock.last_orphan == 0 {
            return Ok(0);
        }
        let mut freed = 0u32;
        let limit = self.geom.inodes_count();
        while self.superblock.last_orphan != 0 && freed <= limit {
            let ino = InodeNum(self.superblock.last_orphan);
            let Ok(inode) = self.read_inode_num(ino) else {
                // An unreadable head cannot be walked past, and guessing would
                // free whatever the rest of the chain happens to name.
                break;
            };
            if inode.links_count != 0 || inode.mode == 0 {
                break;
            }
            self.release_orphan(ino.raw())?;
            freed += 1;
        }
        // Whatever the walk stopped on is no longer something this
        // implementation can drain; clearing the head hands the remainder to
        // `e2fsck` rather than re-walking it on every mount.
        if self.superblock.last_orphan != 0 {
            self.superblock.last_orphan = 0;
            self.superblock_dirty = true;
            self.write_orphan_head()?;
        }
        Ok(freed)
    }

    pub fn orphan_head(&self) -> u32 {
        self.superblock.last_orphan
    }

    /// Move `old_name` under `old_parent` to `new_name` under `new_parent`.
    ///
    /// The new entry is written before the old is removed, so a crash between
    /// the two leaves two names for one inode — recoverable by `e2fsck` —
    /// rather than none. That is the whole of the atomicity ext2 without a
    /// journal can offer.
    pub fn rename_entry(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
    ) -> Result<(), Ext2Error> {
        self.rename_entry_with(old_parent, old_name, new_parent, new_name, LastLink::Free)
            .map(|_| ())
    }

    /// `rename` where `displaced` chooses what happens to an inode the
    /// destination name was the last link of. Answers that inode when it was
    /// orphaned rather than freed.
    ///
    /// A rename over an open file is the same hazard as an `unlink` of one:
    /// POSIX says the displaced inode's contents survive until its last
    /// descriptor closes.
    pub fn rename_entry_with(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
        displaced: LastLink,
    ) -> Result<Option<u32>, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.rename_within(
                InodeNum(old_parent),
                old_name,
                InodeNum(new_parent),
                new_name,
                displaced,
            )
            .map(|o| o.map(|n| n.raw()))
        })
    }

    /// Split into `#[inline(never)]` stages: each holds an `Inode` or two
    /// live, and one frame carrying all of them plus the inlined bodies of
    /// `remove_entry` and the directory helpers exceeds the 2 KiB stack cap.
    fn rename_within(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        new_parent: InodeNum,
        new_name: &[u8],
        displaced_policy: LastLink,
    ) -> Result<Option<InodeNum>, Ext2Error> {
        if new_name.is_empty() || new_name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        for name in [old_name, new_name] {
            if name == b"." || name == b".." {
                return Err(Ext2Error::InvalidPath);
            }
        }
        if old_parent == new_parent && old_name == new_name {
            return Ok(None);
        }

        let Some(plan) = self.rename_plan(old_parent, old_name, new_parent, new_name)? else {
            return Ok(None);
        };

        let mut orphaned = None;
        if let Some(kind) = plan.displaced {
            // POSIX: a directory may only be renamed over an *empty* one, and
            // that check lives in `remove_entry`, ahead of the removal. A
            // directory is never orphaned — `open` refuses one, so no
            // descriptor can be holding it.
            let policy = match kind {
                RemoveKind::Directory => LastLink::Free,
                RemoveKind::NonDirectory => displaced_policy,
            };
            orphaned = self.remove_entry(new_parent, new_name, kind, policy)?;
        }

        self.rename_link_new(new_parent, new_name, &plan)?;
        self.rename_unlink_old(old_parent, old_name, &plan)?;

        if plan.source_is_dir && old_parent != new_parent {
            self.rename_fix_dotdot(plan.source, new_parent)?;
        }

        let mut moved = self.read_inode_num(plan.source)?;
        time::stamp(&mut moved.ctime);
        self.write_inode_num(plan.source, &moved)?;
        self.superblock_dirty = true;

        Ok(orphaned)
    }

    /// Validate a rename and describe what it will do. `Ok(None)` means the
    /// rename is a no-op because both names already denote one inode.
    #[inline(never)]
    fn rename_plan(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        new_parent: InodeNum,
        new_name: &[u8],
    ) -> Result<Option<RenamePlan>, Ext2Error> {
        let source = self
            .lookup_in_dir(old_parent, old_name)?
            .ok_or(Ext2Error::PathNotFound)?;

        let (source_is_dir, file_type) = {
            let source_inode = self.read_inode_num(source)?;
            if source_inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            (source_inode.is_directory(), dir_file_type(&source_inode))
        };

        // Splicing a directory into its own subtree detaches it and everything
        // under it: unreachable from the root, and unremovable because the
        // walk that would find it starts there.
        if source_is_dir && (source == new_parent || self.is_ancestor(source, new_parent)?) {
            return Err(Ext2Error::InvalidPath);
        }

        let existing = self.lookup_in_dir(new_parent, new_name)?;

        let mut displaced = None;
        if let Some(existing) = existing {
            if existing == source {
                return Ok(None);
            }
            let existing_is_dir = self.read_inode_num(existing)?.is_directory();
            match (source_is_dir, existing_is_dir) {
                (true, false) => return Err(Ext2Error::NotDirectory),
                (false, true) => return Err(Ext2Error::IsDirectory),
                _ => {}
            }
            displaced = Some(if existing_is_dir {
                RemoveKind::Directory
            } else {
                RemoveKind::NonDirectory
            });
        }

        Ok(Some(RenamePlan {
            source,
            source_is_dir,
            file_type,
            displaced,
            reparenting: old_parent != new_parent,
        }))
    }

    /// Look a name up in a directory that must be one, and must not be sealed
    /// — the two checks every mutation through a parent needs, kept in one
    /// frame so the caller does not hold the parent `Inode` live across the
    /// rest of its work.
    #[inline(never)]
    fn lookup_in_dir(
        &mut self,
        parent: InodeNum,
        name: &[u8],
    ) -> Result<Option<InodeNum>, Ext2Error> {
        let parent_inode = self.read_inode_num(parent)?;
        if !parent_inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent_inode.is_immutable() {
            return Err(Ext2Error::Immutable);
        }
        Ok(dir::lookup_child(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent.raw()),
        )
        .ok())
    }

    /// Write the new entry. Deliberately before the old one is removed: a
    /// crash here leaves two names for one inode, which `e2fsck` reconciles,
    /// where the other order would leave none and lose the file.
    #[inline(never)]
    fn rename_link_new(
        &mut self,
        new_parent: InodeNum,
        new_name: &[u8],
        plan: &RenamePlan,
    ) -> Result<(), Ext2Error> {
        let mut target_parent = self.read_inode_num(new_parent)?;
        if plan.source_is_dir && plan.reparenting && target_parent.links_count == u16::MAX {
            return Err(Ext2Error::TooManyLinks);
        }
        dir::append_dir_entry(
            &mut target_parent,
            plan.source,
            new_name,
            plan.file_type,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut self.superblock,
            &self.geom,
            BlockOwner::File(new_parent.raw()),
        )?;
        if plan.source_is_dir && plan.reparenting {
            target_parent.links_count += 1;
        }
        time::stamp(&mut target_parent.mtime);
        time::stamp(&mut target_parent.ctime);
        self.write_inode_num(new_parent, &target_parent)
    }

    #[inline(never)]
    fn rename_unlink_old(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        plan: &RenamePlan,
    ) -> Result<(), Ext2Error> {
        // Re-read: `append_dir_entry` may have grown the shared parent when
        // both ends name one directory.
        let source_parent = self.read_inode_num(old_parent)?;
        dir::remove_dir_entry(
            &source_parent,
            old_name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(old_parent.raw()),
        )?;
        let mut source_parent = self.read_inode_num(old_parent)?;
        if plan.source_is_dir && plan.reparenting {
            source_parent.links_count = source_parent.links_count.saturating_sub(1);
        }
        time::stamp(&mut source_parent.mtime);
        time::stamp(&mut source_parent.ctime);
        self.write_inode_num(old_parent, &source_parent)
    }

    #[inline(never)]
    fn rename_fix_dotdot(
        &mut self,
        moved: InodeNum,
        new_parent: InodeNum,
    ) -> Result<(), Ext2Error> {
        let inode = self.read_inode_num(moved)?;
        dir::update_dotdot(
            &inode,
            new_parent,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(moved.raw()),
        )
    }

    /// Whether `ancestor` is on the `..` chain above `descendant`. Bounded by
    /// the inode count, so a `..` cycle in a damaged image terminates.
    fn is_ancestor(&mut self, ancestor: InodeNum, descendant: InodeNum) -> Result<bool, Ext2Error> {
        let mut current = descendant;
        let mut steps = 0u32;
        let limit = self.geom.inodes_count();
        while current != InodeNum::ROOT {
            if current == ancestor {
                return Ok(true);
            }
            steps += 1;
            if steps > limit {
                return Err(Ext2Error::DirectoryFormat);
            }
            let inode = self.read_inode_num(current)?;
            let parent = dir::lookup_child(
                &inode,
                b"..",
                &mut *self.cache,
                self.device,
                self.ptrs_per_block,
                self.block_size,
                BlockOwner::File(current.raw()),
            )?;
            if parent == current {
                break;
            }
            current = parent;
        }
        Ok(current == ancestor)
    }

    pub fn remove_path(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        let (parent_path, name) = ondisk::split_parent(path).ok_or(Ext2Error::PathNotFound)?;
        let parent = self.resolve_path(parent_path)?;
        self.unlink_entry(parent, name)
    }

    fn read_group_desc(&mut self, group: GroupIdx) -> Result<GroupDesc, Ext2Error> {
        ext2_alloc::read_group_desc(group, &self.geom, self.cache, self.device)
    }

    /// Write an inode record verbatim, so a test can construct on-disk states
    /// this implementation has no operation for — a second hard link, above
    /// all, which every other ext2 writer produces and `unlink` must honour.
    #[cfg(feature = "tests")]
    pub fn write_inode_for_test(&mut self, ino: u32, inode: &Inode) -> Result<(), Ext2Error> {
        self.write_inode_num(InodeNum(ino), inode)
    }

    /// A group's directory count, which `e2fsck` cross-checks against the
    /// inodes it finds.
    pub fn group_used_dirs(&mut self, group: u32) -> Result<u16, Ext2Error> {
        let group = self.geom.group(group).ok_or(Ext2Error::InvalidBlock)?;
        Ok(self.read_group_desc(group)?.used_dirs_count)
    }

    /// Free every block an inode owns: the twelve direct ones and all three
    /// indirect trees. Missing depths 2 and 3 leaks every block past the
    /// single-indirect reach on each delete, with no way to recover the space
    /// short of reformatting.
    fn release_file_blocks(&mut self, inode: &Inode, owner: BlockOwner) -> Result<(), Ext2Error> {
        for blk in inode.block.iter().take(12) {
            if blk.is_valid() {
                ext2_alloc::free_block(
                    *blk,
                    &self.geom,
                    &mut self.superblock,
                    &mut *self.cache,
                    self.device,
                )?;
            }
        }

        let mut freed: slopos_ostd::KVec<BlockNum> = slopos_ostd::KVec::new();
        for (slot, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let root = inode.block[slot];
            if !root.is_valid() {
                continue;
            }
            // `free_indirect` hands back every block it walks; the frees are
            // applied afterwards because the superblock and the cache it
            // needs are borrowed for the walk.
            file::free_indirect(
                root,
                depth,
                &mut self.cache,
                self.device,
                self.ptrs_per_block,
                owner,
                &mut |b| {
                    freed.push(b).map_err(|_| Ext2Error::OutOfMemory)?;
                    Ok(())
                },
            )?;
            freed.push(root).map_err(|_| Ext2Error::OutOfMemory)?;
        }

        for blk in freed.iter() {
            ext2_alloc::free_block(
                *blk,
                &self.geom,
                &mut self.superblock,
                &mut *self.cache,
                self.device,
            )?;
        }

        Ok(())
    }

    #[inline(never)]
    fn write_superblock(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.superblock.encode_mutable_fields(&mut sb_buf);
        ondisk::SuperblockBookkeeping::stamp_write(&mut sb_buf, time::now_unix_opt());
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(())
    }
}
