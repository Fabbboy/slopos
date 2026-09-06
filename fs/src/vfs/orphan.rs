//! Open-inode references, and the inodes whose last name is gone while a
//! reference still holds them.
//!
//! POSIX requires an unlinked-but-open file to keep its contents until the
//! last close. Doing that needs two halves. The on-disk half is the
//! filesystem's — ext2 threads the inode onto `s_last_orphan`, so a crash
//! leaves a list the next `e2fsck` drains rather than leaked blocks. The
//! in-memory half is here: a count of how many open vnodes name each inode,
//! which is what says *when* the deferred free may run.
//!
//! Two properties are load-bearing.
//!
//! **A reference is taken before the descriptor is usable and released after
//! it is not.** `unlink` consults the count under the same lock the reference
//! is taken under, so an inode with a live descriptor is never freed and an
//! inode with none is freed at once rather than being left to the next mount.
//! A reference that cannot be recorded fails the *open* (`ENFILE`) rather than
//! being skipped: an untracked descriptor reads as an unreferenced inode, and
//! that is the one error that frees a file somebody is reading.
//!
//! **The free does not run at the last close.** A descriptor is dropped from
//! `Drop`, which on the task-exit path runs under a preempt guard, and freeing
//! an inode takes a sleeping mutex and parks on block I/O. The last close only
//! marks the record releasable; the filesystem's own writeback thread and the
//! shutdown path drain it. The cost is that the space comes back a flusher
//! tick late, which is what a deferred free means.
//!
//! What this does *not* close is the window between a path walk resolving an
//! inode and the `open` installing its reference: an `unlink` landing in
//! between still frees underneath it. Closing that needs an inode cache keyed
//! by `(filesystem, inode)` and a parent-directory lock held across lookup and
//! install, neither of which exists here. It is strictly narrower than the
//! behaviour it replaces, where every `unlink` freed regardless of who was
//! reading.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::KVec;
use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, SpinLock};

use crate::vfs::traits::{FileSystem, InodeId, same_filesystem};

/// Tracked inodes. An entry exists while an inode is open, or detached and
/// waiting to be freed, so this bounds neither open files nor unlinked ones
/// on its own — it is the ceiling past which an open fails rather than going
/// untracked.
///
/// Above the system-wide open-vnode limit, so exhausting this needs more open
/// *inodes* than there are open vnodes, which cannot happen through the
/// descriptor path alone.
const MAX_TRACKED: usize = 2048;

struct Tracked {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    /// Open vnodes naming this inode.
    refs: u32,
    /// A removal naming this inode is in flight between [`begin_removal`] and
    /// [`end_removal`]. While it is, no free may be queued: the filesystem is
    /// still mutating the inode, and a concurrent last close that queued one
    /// would have the deferred free race the removal that decided on it.
    removing: bool,
    /// The last name is gone; the filesystem owes a deferred free.
    detached: bool,
    /// Detached with no references left, so the free may run.
    releasable: bool,
}

/// A *spinning* lock, and it must stay one: every operation is a bounded scan
/// with no I/O in it, and the release path runs from a `Drop` that the
/// task-exit path reaches under a preempt guard.
static TRACKED: SpinLock<KVec<Tracked>> = SpinLock::new(
    KVec::new(),
    lock_class!("VFS_OPEN_INODES", LOCK_LEVEL_RESOURCE),
);

/// Mirrors the number of releasable records, so a writeback thread's wait
/// predicate reads one relaxed atomic instead of taking the lock above.
static RELEASABLE: AtomicUsize = AtomicUsize::new(0);

/// One-shot, so the degraded-mode line below is reported rather than repeated.
static PLACEHOLDER_REFUSED: InitFlag = InitFlag::new();

/// One-shot, for a filesystem that defers frees with nothing to run them.
static UNDRAINABLE_REPORTED: InitFlag = InitFlag::new();

fn find(table: &KVec<Tracked>, fs: &'static dyn FileSystem, inode: InodeId) -> Option<usize> {
    table
        .iter()
        .position(|e| e.inode == inode && same_filesystem(e.fs, fs))
}

/// Drop an entry that holds neither a reference nor an obligation.
fn prune(table: &mut KVec<Tracked>, idx: usize) {
    if table[idx].refs == 0 && !table[idx].detached {
        table.swap_remove(idx);
    }
}

/// Why [`open_ref`] refused. The two reasons are different errors to the
/// caller: a name that is gone is `ENOENT`, and a table with no room is
/// `ENFILE`. Collapsing them would tell a process racing an `unlink` that the
/// system-wide file table was exhausted, and invite it to retry forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRefError {
    /// The inode's last name is gone, or is being removed right now.
    Detached,
    /// No room to track another inode.
    TableFull,
}

/// Take a reference for a newly opened vnode. The caller must turn an `Err`
/// into a failed open.
pub fn open_ref(fs: &'static dyn FileSystem, inode: InodeId) -> Result<(), OpenRefError> {
    let mut table = TRACKED.lock();
    if let Some(idx) = find(&table, fs, inode) {
        // A detached inode reached through a path is a lookup that raced the
        // unlink. Refusing is what `open(2)` reports for a name that is gone,
        // and it keeps a reference off a record whose free may already be in
        // flight.
        if table[idx].detached {
            return Err(OpenRefError::Detached);
        }
        table[idx].refs += 1;
        return Ok(());
    }
    if table.len() >= MAX_TRACKED {
        return Err(OpenRefError::TableFull);
    }
    table
        .push(Tracked {
            fs,
            inode,
            refs: 1,
            removing: false,
            detached: false,
            releasable: false,
        })
        .map_err(|_| OpenRefError::TableFull)
}

/// Release a vnode's reference. The deferred free becomes runnable when the
/// last reference on a detached inode goes.
///
/// Answers whether that just happened, so the caller can run the free inline
/// (a filesystem whose free cannot block) or wake the thread that will.
pub fn close_ref(fs: &'static dyn FileSystem, inode: InodeId) -> bool {
    let mut table = TRACKED.lock();
    let Some(idx) = find(&table, fs, inode) else {
        return false;
    };
    table[idx].refs = table[idx].refs.saturating_sub(1);
    // `removing` is what keeps this from queueing a free the in-flight removal
    // has not finished deciding on; `end_removal` re-checks the count and
    // queues it then.
    if table[idx].refs == 0 && table[idx].detached && !table[idx].removing && !table[idx].releasable
    {
        table[idx].releasable = true;
        RELEASABLE.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    prune(&mut table, idx);
    false
}

/// Claim `inode` for an in-flight removal, answering how the removal must
/// treat it.
///
/// Marks the inode detached *before* the removal runs, which is what closes
/// the window: [`open_ref`] refuses a detached inode, so the
/// [`DetachPlan::FreeNow`] answer cannot go stale between this call and the
/// removal it decides. An inode with no entry gets a placeholder for exactly
/// that reason — without one, a concurrent open between this call and the
/// free would install a reference the free never sees, and the descriptor
/// would be left naming a reclaimed inode.
///
/// Every path must reach [`end_removal`], which either keeps the record (a
/// real deferral) or drops it. A placeholder that could not be allocated
/// still answers [`DetachPlan::FreeNow`]: the race it would have closed is
/// the one that already existed, and failing the `unlink` because the kernel
/// is out of memory is worse than the window.
pub fn begin_removal(fs: &'static dyn FileSystem, inode: InodeId) -> DetachPlan {
    let mut table = TRACKED.lock();
    if let Some(idx) = find(&table, fs, inode) {
        table[idx].detached = true;
        table[idx].removing = true;
        return if table[idx].refs == 0 {
            DetachPlan::FreeNow
        } else {
            DetachPlan::Deferred
        };
    }
    let placed = table.len() < MAX_TRACKED
        && table
            .push(Tracked {
                fs,
                inode,
                refs: 0,
                removing: true,
                detached: true,
                releasable: false,
            })
            .is_ok();
    drop(table);
    if !placed {
        // Once per boot: the condition that produces this produces it in bulk.
        if PLACEHOLDER_REFUSED.init_once() {
            klog_info!(
                "vfs: open-inode table full — a concurrent open of an inode being \
                 unlinked is no longer refused; unlink and open may race"
            );
        }
    }
    DetachPlan::FreeNow
}

/// What a removal must do with the inode whose last name it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachPlan {
    /// Nothing holds the inode open: free it with the name.
    FreeNow,
    /// A reference holds it: keep the contents and defer the free.
    Deferred,
}

/// How a removal that [`begin_removal`] answered [`DetachPlan::Deferred`] for
/// actually turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// The filesystem deferred the free of this inode.
    Deferred,
    /// Nothing was deferred: the removal failed, the inode had other links, or
    /// the filesystem freed it outright.
    Nothing,
}

/// Close the scope [`begin_removal`] opened.
///
/// [`RemovalOutcome::Nothing`] drops the claim, so an inode that kept a name
/// is openable again. [`RemovalOutcome::Deferred`] keeps the record until the
/// last reference goes.
///
/// Answers whether a deferred free became runnable here, which happens when a
/// close landed inside the removal scope. The caller must then run or wake the
/// drain, because the close that would ordinarily have done so was refused by
/// `removing` and will not come back.
#[must_use = "a free that became runnable here needs the caller to drain it"]
pub fn end_removal(fs: &'static dyn FileSystem, inode: InodeId, outcome: RemovalOutcome) -> bool {
    let mut table = TRACKED.lock();
    let Some(idx) = find(&table, fs, inode) else {
        return false;
    };
    table[idx].removing = false;
    match outcome {
        RemovalOutcome::Nothing => {
            table[idx].detached = false;
            prune(&mut table, idx);
            false
        }
        RemovalOutcome::Deferred => {
            table[idx].detached = true;
            // A close that landed inside the removal scope leaves the entry at
            // zero references with the free still owed, and nothing else comes
            // back for it — `close_ref` deliberately queued nothing while
            // `removing` was set. This is where that free is queued.
            if table[idx].refs == 0 && !table[idx].releasable {
                table[idx].releasable = true;
                RELEASABLE.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            false
        }
    }
}

/// Run or wake whatever completes `fs`'s deferred frees.
///
/// The filesystem chooses. One whose free cannot block has the drain run
/// inline, because nothing else would ever run it. One whose free *can* block
/// is handed to its own writeback thread — and today ext2 is the only
/// filesystem with one, so this asks the filesystem to say so rather than
/// assuming it.
///
/// A blocking filesystem with no writeback thread would leak: its records
/// would stay releasable forever, and `releasable_count()` staying nonzero
/// would also keep ext2's flusher awake every interval. That is a wiring
/// mistake rather than a runtime condition, so it is reported once and the
/// obligation is handed back to the on-disk list, which the next mount drains.
///
/// The caller must hold no filesystem lock: the inline arm reaches
/// [`FileSystem::release_detached`].
pub fn drain_or_wake(fs: &'static dyn FileSystem) {
    if !fs.release_detached_blocks() {
        drain_releasable(fs);
        return;
    }
    if fs.wake_for_detached() {
        return;
    }
    if UNDRAINABLE_REPORTED.init_once() {
        klog_info!(
            "vfs: filesystem '{}' defers inode frees but has no writeback thread to \
             run them — they are left for its next mount to reclaim",
            fs.name()
        );
    }
    // Drop the claims rather than leave them counted: nothing is coming for
    // them, and a permanently nonzero `releasable_count()` would spin every
    // other filesystem's flusher.
    forget_filesystem(fs);
}

/// How many records are waiting to be freed. Read by a writeback thread's wait
/// predicate, which must take no lock.
pub fn releasable_count() -> usize {
    RELEASABLE.load(Ordering::Relaxed)
}

/// Claim one releasable record naming `fs`. Exactly one caller can claim a
/// given record, so a deferred free runs once.
fn take_releasable(fs: &'static dyn FileSystem) -> Option<InodeId> {
    let mut table = TRACKED.lock();
    let idx = table
        .iter()
        .position(|e| e.releasable && same_filesystem(e.fs, fs))?;
    let entry = table.swap_remove(idx);
    RELEASABLE.fetch_sub(1, Ordering::Relaxed);
    Some(entry.inode)
}

/// Complete every deferred free `fs` owes. Answers how many inodes it freed.
///
/// Takes whatever locks the filesystem takes, so the caller must hold none —
/// in particular not the mount lock the free itself needs. A failure drops the
/// record rather than re-queueing it: the on-disk orphan list still names the
/// inode, so the next mount reclaims it, whereas re-queueing a record whose
/// filesystem is refusing writes would spin the caller.
pub fn drain_releasable(fs: &'static dyn FileSystem) -> usize {
    let mut freed = 0usize;
    while let Some(inode) = take_releasable(fs) {
        if fs.release_detached(inode).is_ok() {
            freed += 1;
        }
    }
    freed
}

/// Drop every record naming `fs`, for a filesystem going away under a fixture
/// reset. The on-disk list is what carries the obligation across a mount.
pub fn forget_filesystem(fs: &'static dyn FileSystem) {
    let mut table = TRACKED.lock();
    let mut i = table.len();
    while i > 0 {
        i -= 1;
        if same_filesystem(table[i].fs, fs) {
            if table[i].releasable {
                RELEASABLE.fetch_sub(1, Ordering::Relaxed);
            }
            table.swap_remove(i);
        }
    }
}

/// Whether any tracked record naming `fs` still holds an open reference.
///
/// `umount2`'s busy test. A record that is merely detached does not count: it
/// is an obligation the unmount discharges, not a descriptor that would
/// observe the filesystem going away.
pub fn has_open_refs(fs: &'static dyn FileSystem) -> bool {
    let table = TRACKED.lock();
    table
        .iter()
        .any(|e| e.refs > 0 && same_filesystem(e.fs, fs))
}

/// Tracked inodes: open, detached, or both.
pub fn tracked_count() -> usize {
    TRACKED.lock().len()
}
