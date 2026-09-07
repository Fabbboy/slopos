//! The metadata redo log, and the bounded writeback pass it makes safe.
//!
//! The fixtures build the log the way the image builder does — a preallocated
//! sealed file at `/.journal` — so these tests exercise the same attach path a
//! boot takes rather than a synthetic one.
//!
//! Every mount goes through [`with_log`] and the probe device reports through
//! statics rather than a handle the bodies would carry: one `Ext2Fs` plus one
//! `BlockCache` already fills a 2 KiB frame.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use slopos_ostd::KVec;
use slopos_testing::{TestResult, fail};

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, build_ext2_image};
use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Fs, JOURNAL_PATH, ReadOnlyReason};

/// Comfortably above `journal::MIN_LOG_SLOTS`, and small enough to leave the
/// fixture room to allocate.
const LOG_BLOCKS: u32 = 48;
const PAYLOAD: &[u8] = b"a transaction the home locations never saw";

static PROBE_WRITES: AtomicUsize = AtomicUsize::new(0);
static PROBE_FLUSHES: AtomicUsize = AtomicUsize::new(0);
/// Offset of the first write since the counters were cleared, so a test can
/// assert on write *order* and not only on volume.
static PROBE_FIRST: AtomicU64 = AtomicU64::new(u64::MAX);
static PROBE_REFUSES: AtomicBool = AtomicBool::new(false);

/// Counts writes and can be made to refuse them part-way through a test.
/// Refusing only once armed is what the retraction test needs: the log must
/// attach before the failure it is measuring.
struct ProbeDevice {
    inner: MemoryBlockDevice,
}

impl ProbeDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        PROBE_WRITES.store(0, Ordering::Relaxed);
        PROBE_FLUSHES.store(0, Ordering::Relaxed);
        PROBE_FIRST.store(u64::MAX, Ordering::Relaxed);
        PROBE_REFUSES.store(false, Ordering::Relaxed);
        Self { inner }
    }
}

impl BlockDevice for ProbeDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if PROBE_REFUSES.load(Ordering::Relaxed) {
            return Err(BlockDeviceError::InvalidBuffer);
        }
        PROBE_WRITES.fetch_add(1, Ordering::Relaxed);
        let _ =
            PROBE_FIRST.compare_exchange(u64::MAX, offset, Ordering::Relaxed, Ordering::Relaxed);
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        PROBE_FLUSHES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn plain_image() -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks: 512,
        inodes: 32,
        file_name: None,
        file_data: None,
        file_block: FIX_FILE_BLOCK,
    })
}

/// Mount `device` and hand the handle to `body`. Its own frame, as
/// `tests::with_mounted` is.
#[inline(never)]
fn with_log(
    device: &dyn BlockDevice,
    body: fn(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).map_err(|_| "mount_params")?;
    let mut cache = BlockCache::new_boxed(bs).map_err(|_| "cache")?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).map_err(|_| "mount")?;
    body(&mut fs)
}

/// A fixture carrying a log, built through the ordinary write path.
fn journal_image() -> Option<MemoryBlockDevice> {
    let device = plain_image()?;
    match with_log(&device, install_log) {
        Ok(()) => Some(device),
        Err(_) => None,
    }
}

fn install_log(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let bs = fs.block_size();
    let ino = fs
        .create_file(2, &JOURNAL_PATH[1..])
        .map_err(|_| "create")?;
    let zeros = KVec::<u8>::zeroed(bs as usize).map_err(|_| "zeros")?;
    for index in 0..LOG_BLOCKS {
        fs.write_file(ino, u64::from(index) * u64::from(bs), zeros.as_slice())
            .map_err(|_| "preallocate")?;
    }
    fs.set_sealed(ino).map_err(|_| "seal")?;
    fs.sync().map_err(|_| "sync")?;
    fs.mark_clean().map_err(|_| "clean")
}

fn attach(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    match fs.attach_journal() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("the fixture's log was not attached"),
        Err(_) => Err("attach failed"),
    }
}

/// An operation the log recorded and nothing else did survives a mount that
/// never saw a clean unmount — the whole point of having a log.
pub fn test_ext2_journal_replays_an_unsynced_operation() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, log_one_operation) {
        return fail!("staging the transaction: {}", msg);
    }
    let Ok((sb, ..)) = Ext2Fs::mount_params(&device) else {
        return fail!("the staged image no longer parses");
    };
    if Ext2Fs::mount_read_only_reason(&sb, &device) != Some(ReadOnlyReason::NotCleanlyUnmounted) {
        return fail!("the image should read as never cleanly unmounted");
    }
    match with_log(&device, replay_and_check) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn log_one_operation(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    // What a boot does: stamp the image not-clean, then attach.
    fs.mark_dirty_on_disk().map_err(|_| "not-clean stamp")?;
    attach(fs)?;
    let ino = fs.create_file(2, b"logged.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    // Deliberately no sync. The commit record is on the medium and the home
    // locations are not, which is exactly what a power cut here leaves.
    Ok(())
}

fn replay_and_check(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let recovery = match fs.attach_journal() {
        Ok(Some(recovery)) => recovery,
        Ok(None) => return Err("no log on the remount"),
        Err(_) => return Err("attach failed on the remount"),
    };
    if !recovery.replayed() {
        return Err("the log held a committed transaction and replayed nothing");
    }
    let ino = fs
        .resolve_path(b"/logged.txt")
        .map_err(|_| "the replay did not restore the name")?;
    let mut buf = [0u8; 64];
    let read = fs.read_file(ino, 0, &mut buf).map_err(|_| "read back")?;
    if &buf[..read] != PAYLOAD {
        return Err("the replayed file holds the wrong bytes");
    }
    Ok(())
}

/// A commit that cannot reach the medium leaves neither the log nor the
/// filesystem carrying half an operation.
pub fn test_ext2_journal_retracts_a_failed_commit() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, retract_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn retract_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let head = fs.journal_head();
    PROBE_REFUSES.store(true, Ordering::Relaxed);
    let created = fs.create_file(2, b"doomed.txt").is_ok();
    PROBE_REFUSES.store(false, Ordering::Relaxed);
    if created {
        return Err("a create whose commit could not be written reported success");
    }
    if fs.journal_head() != head {
        return Err("the log kept the records of a retracted operation");
    }
    if fs.dirty_count() != 0 {
        return Err("the cache kept blocks a retracted operation dirtied");
    }
    if fs.resolve_path(b"/doomed.txt").is_ok() {
        return Err("the retracted name is still resolvable");
    }
    Ok(())
}

/// The log's own file is kernel state: not readable, not removable.
pub fn test_ext2_journal_file_is_not_userland_data() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    match with_log(&device, guard_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn guard_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.resolve_path(JOURNAL_PATH).map_err(|_| "resolve")?;
    let mut buf = [0u8; 32];
    if fs.read_file(ino, 0, &mut buf).is_ok() {
        return Err("the log's blocks are readable through the VFS");
    }
    if fs.unlink_entry(2, &JOURNAL_PATH[1..]).is_ok() {
        return Err("the log's file can be unlinked from under the mount");
    }
    if fs.truncate_file(ino, 0).is_ok() {
        return Err("the log's file can be truncated from under the mount");
    }
    Ok(())
}

/// A writeback step writes at most its budget and a pass driven one write at a
/// time still finishes — which is what bounds the wait behind `sync(2)`.
pub fn test_ext2_writeback_step_respects_its_budget() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, budget_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn budget_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    for name in [&b"a.txt"[..], b"b.txt", b"c.txt", b"d.txt"] {
        let ino = fs.create_file(2, name).map_err(|_| "create")?;
        fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    }

    let mut pass = fs.begin_sync();
    let mut steps = 0usize;
    while !pass.is_done() {
        let before = PROBE_WRITES.load(Ordering::Relaxed);
        fs.sync_step(&mut pass, 1).map_err(|_| "step")?;
        if PROBE_WRITES.load(Ordering::Relaxed) - before > 1 {
            return Err("a step wrote more blocks than its budget");
        }
        steps += 1;
        if steps > 4096 {
            return Err("the pass never reached its end");
        }
    }
    if steps < 4 {
        return Err("the fixture produced too little to bound");
    }
    if fs.sync_pending() {
        return Err("the pass finished with work outstanding");
    }
    Ok(())
}

/// A pass writes what was dirty when it opened and nothing an operation
/// dirtied behind it. Without that, releasing the mount lock between steps
/// would publish metadata ahead of a later operation's data.
pub fn test_ext2_writeback_leaves_a_later_operation_alone() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    match with_log(&device, epoch_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn epoch_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"first.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    drive(fs, fs.begin_sync())?;
    if fs.sync_pending() {
        return Err("an undisturbed pass left work behind");
    }

    // The same again, with an operation landing after the pass opened.
    let ino = fs.create_file(2, b"second.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    let pass = fs.begin_sync();
    let late = fs.create_file(2, b"third.txt").map_err(|_| "create")?;
    fs.write_file(late, 0, PAYLOAD).map_err(|_| "write")?;
    drive(fs, pass)?;
    if !fs.sync_pending() {
        return Err("the pass published an operation that started after it");
    }
    fs.sync().map_err(|_| "second sync")?;
    if fs.sync_pending() {
        return Err("the following pass did not take the late operation");
    }
    Ok(())
}

fn drive(fs: &mut Ext2Fs<'_>, mut pass: crate::ext2::SyncPass) -> Result<(), &'static str> {
    let mut steps = 0usize;
    while !pass.is_done() {
        fs.sync_step(&mut pass, 8).map_err(|_| "step")?;
        steps += 1;
        if steps > 4096 {
            return Err("the pass never reached its end");
        }
    }
    Ok(())
}

/// A block whose only current copy is a log record reaches its home location.
///
/// The path that broke: a cache miss served from the log left the entry
/// *clean*, so the pass had nothing to write, the check point skipped it (the
/// cache said the home matched) and the reset dropped the only copy. Every
/// read still answered correctly until the mount went away.
pub fn test_ext2_journal_checkpoints_a_block_read_back_from_the_log() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, log_then_reread) {
        return fail!("{}", msg);
    }
    let Ok((sb, ..)) = Ext2Fs::mount_params(&device) else {
        return fail!("the image no longer parses");
    };
    if let Some(reason) = Ext2Fs::mount_read_only_reason(&sb, &device) {
        return fail!("the image should have been left clean, got {:?}", reason);
    }
    match with_log(&device, expect_survivor) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn log_then_reread(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"keep.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;

    // A failed operation drops every entry it touched, so the committed
    // contents of those blocks live only in the log.
    if fs.create_file(2, b"keep.txt").is_ok() {
        return Err("a duplicate create succeeded");
    }
    // ... and this read brings them back from it.
    let found = fs.resolve_path(b"/keep.txt").map_err(|_| "resolve")?;
    if found != ino {
        return Err("the re-read resolved to a different inode");
    }

    fs.sync().map_err(|_| "sync")?;
    if fs.sync_pending() {
        return Err("the sync left work behind");
    }
    fs.mark_clean().map_err(|_| "clean stamp")
}

fn expect_survivor(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let recovery = match fs.attach_journal() {
        Ok(Some(recovery)) => recovery,
        Ok(None) => return Err("no log on the remount"),
        Err(_) => return Err("attach failed on the remount"),
    };
    if recovery.replayed() {
        return Err("the log still held a transaction a clean unmount should have drained");
    }
    let ino = fs
        .resolve_path(b"/keep.txt")
        .map_err(|_| "the check point never reached the home locations")?;
    let mut buf = [0u8; 64];
    let read = fs.read_file(ino, 0, &mut buf).map_err(|_| "read back")?;
    if &buf[..read] != PAYLOAD {
        return Err("the survivor holds the wrong bytes");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_replays_an_unsynced_operation,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_retracts_a_failed_commit,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_checkpoints_a_block_read_back_from_the_log,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_file_is_not_userland_data,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_writeback_step_respects_its_budget,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_writeback_leaves_a_later_operation_alone,
    suite = fs
);

/// An `unlink` of a file something still holds open must publish nothing to a
/// home location before its transaction commits.
///
/// The path that broke: the orphan list got its ordering by flushing the
/// member's record *home* mid-operation, which under a log publishes
/// uncommitted metadata that the rollback then drops rather than repairs — so
/// a later failure left a live name pointing at `links_count == 0`.
///
/// Observed as the barrier count: a mid-operation home write barriers behind
/// itself, so the old path issued two where the log path issues one.
pub fn test_ext2_journal_orphan_publishes_nothing_before_the_commit() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, orphan_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn orphan_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"orphan.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    fs.sync().map_err(|_| "sync")?;

    PROBE_FLUSHES.store(0, Ordering::Relaxed);
    let detached = fs
        .detach_entry(2, b"orphan.txt")
        .map_err(|_| "detach")?
        .ok_or("the last link did not orphan the inode")?;
    if detached != ino {
        return Err("a different inode was orphaned");
    }
    let barriers = PROBE_FLUSHES.load(Ordering::Relaxed);
    if barriers != 1 {
        return Err("the orphan path barriered more than once, so it published home early");
    }
    // And the list is still usable: the deferred head write landed.
    fs.release_orphan(detached).map_err(|_| "release")?;
    if fs.resolve_path(b"/orphan.txt").is_ok() {
        return Err("the detached name is still resolvable");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_orphan_publishes_nothing_before_the_commit,
    suite = fs
);

/// Unthreading an orphan publishes the list head *before* the commit that
/// destroys the member's chain link.
///
/// The two directions need opposite orderings. A push wants the member's
/// `i_dtime` recoverable before the head names it, so it defers; a *removal*
/// shares a transaction with the free that overwrites that `i_dtime`, so a
/// deferred head would name an inode whose chain link is already gone and the
/// next mount's drain would discard every orphan behind it.
///
/// Observed as the offset of the first device write: the head is at byte 1024,
/// and a deferred one would put a log block first.
pub fn test_ext2_journal_orphan_removal_publishes_the_head_first() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, orphan_removal_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn orphan_removal_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"unthread.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    let detached = fs
        .detach_entry(2, b"unthread.txt")
        .map_err(|_| "detach")?
        .ok_or("the last link did not orphan the inode")?;
    fs.sync().map_err(|_| "sync")?;

    PROBE_FIRST.store(u64::MAX, Ordering::Relaxed);
    fs.release_orphan(detached).map_err(|_| "release")?;
    if PROBE_FIRST.load(Ordering::Relaxed) != 1024 {
        return Err("the removal logged before it published the head");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_orphan_removal_publishes_the_head_first,
    suite = fs
);
