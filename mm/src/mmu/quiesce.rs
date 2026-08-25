//! TLB quiesce epochs — a frame is not reused until every CPU has invalidated.
//!
//! The obligation is discharged at free time, not allocation time: `alloc` is
//! reachable from page-fault handlers and from inside interrupt-disabling
//! locks, where a CPU cannot service the IPI the allocating CPU would wait for.
//! Nothing here ever waits on a peer; a wedged one grows the quarantine and, in
//! the limit, fails an allocation.
//!
//! A CPU *acks* an epoch by doing a local all-context invalidation and
//! publishing the epoch it flushed at; all online CPUs acked ⇒ the epoch
//! advances.
//!
//! A frame freed during epoch `E` is safe at `E + 2`, not `E + 1`: acking `E`
//! proves nothing, because a CPU may have acked early, *before* the unmap. Only
//! acks of `E + 1` are ordered after the advance to `E + 1`, which is itself
//! ordered after the free's read of `E`.
//!
//! Advancing is demand-driven, not periodic — an ack costs the full local TLB,
//! so it is amortised over a batch ([`request_advance`]) with a slow tick
//! backstop for an idle system.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use slopos_arch::pcr::MAX_CPUS;

use super::asid;

/// Starts at 1 so a stamped epoch is distinguishable from a zeroed slot.
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// Highest epoch each CPU has flushed at. `0` = never acked, i.e. not booted.
static CPU_ACKED: [AtomicU64; MAX_CPUS] = {
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; MAX_CPUS]
};

/// Until the tick runs nothing can ack, so quarantining would park frames with
/// no way to release them. Early boot frees straight through.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Most recent epoch in which any CPU deferred an invalidation.
///
/// An epoch stamp, not a "something was deferred" flag: unmap and free are
/// separated in time for anything refcounted (COW, fork, memfd, rings), so a
/// flag cleared each advance already reads false by the time the frame is
/// freed — releasing it while a peer still resolves its old address.
static LAST_DEFERRED_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Pending request to close the current epoch, set by [`request_advance`].
static ADVANCE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ticks with a non-empty quarantine and no advance requested.
static IDLE_TICKS: AtomicUsize = AtomicUsize::new(0);

/// A quarter second at 100 Hz: long enough that a busy system trips the
/// occupancy watermark first, short enough that an idle one returns memory.
const IDLE_TICK_LIMIT: usize = 25;

/// Arm the epoch machinery. Called once the periodic timer is delivering ticks.
pub fn activate() {
    ACTIVE.store(true, Ordering::Release);
}

/// Is the epoch machinery live? While false the allocator frees directly.
#[inline]
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// The epoch a frame freed right now should be stamped with.
#[inline]
pub fn current_epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

/// Record that an invalidation was deferred rather than broadcast, returning
/// the epoch it stamped — a re-read of [`current_epoch`] can already name a
/// later one.
#[inline]
pub fn note_deferred_unmap() -> u64 {
    let epoch = EPOCH.load(Ordering::Acquire);
    // Monotone: a concurrent advance must not walk the stamp backwards and
    // shorten the window a frame is protected for.
    LAST_DEFERRED_EPOCH.fetch_max(epoch, Ordering::AcqRel);
    epoch
}

/// Keying on the newest deferral system-wide rather than per frame
/// over-quarantines, costing memory; the alternative costs correctness.
#[inline]
pub fn quarantine_required() -> bool {
    if !is_active() {
        return false;
    }
    let epoch = EPOCH.load(Ordering::Acquire);
    epoch
        < LAST_DEFERRED_EPOCH
            .load(Ordering::Acquire)
            .saturating_add(2)
}

/// Idempotent; the flushes happen on each CPU's own schedule.
#[inline]
pub fn request_advance() {
    ADVANCE_REQUESTED.store(true, Ordering::Release);
}

/// Per-CPU quiesce point; two atomic loads unless an advance is pending.
///
/// Legal from a hard IRQ handler: no lock, no allocation, no wait.
pub fn tick() {
    if !is_active() {
        return;
    }
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu >= MAX_CPUS {
        return;
    }

    let epoch = EPOCH.load(Ordering::Acquire);
    if !ADVANCE_REQUESTED.load(Ordering::Acquire) {
        backstop(cpu);
        return;
    }

    if CPU_ACKED[cpu].load(Ordering::Relaxed) < epoch {
        // The ack must not be visible before the invalidation it certifies.
        asid::flush_local_all_contexts();
        CPU_ACKED[cpu].store(epoch, Ordering::Release);
    }

    try_close(epoch);
}

/// Ack immediately rather than waiting for this CPU's next tick. Still needs
/// the peers to close, so it can return with nothing released.
pub fn ack_now() {
    if !is_active() {
        return;
    }
    request_advance();
    tick();
}

/// Close `epoch` if every online CPU has acked it, releasing the batch of
/// frames that became safe. Reports whether this call was the one that closed
/// it, so a caller learns the outcome without re-reading a counter its peers
/// also advance.
fn try_close(epoch: u64) -> bool {
    for cpu in 0..MAX_CPUS {
        if !slopos_arch::pcr::is_cpu_online(cpu) {
            continue;
        }
        if CPU_ACKED[cpu].load(Ordering::Acquire) < epoch {
            return false;
        }
    }

    // The CAS winner rotates for everyone; losers have nothing left to do.
    if EPOCH
        .compare_exchange(epoch, epoch + 1, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }

    ADVANCE_REQUESTED.store(false, Ordering::Release);
    IDLE_TICKS.store(0, Ordering::Relaxed);

    crate::page_alloc::quarantine_rotate();
    true
}

/// Advance a quarantine too small to trip the occupancy watermark.
fn backstop(cpu: usize) {
    if cpu != 0 || !crate::page_alloc::quarantine_is_occupied() {
        return;
    }
    if IDLE_TICKS.fetch_add(1, Ordering::Relaxed) + 1 >= IDLE_TICK_LIMIT {
        IDLE_TICKS.store(0, Ordering::Relaxed);
        request_advance();
    }
}

/// Acks on behalf of every online CPU — a lie about their TLBs, and therefore
/// test-only. Lets the window logic be checked without racing AP scheduling.
///
/// Returns the epoch this call closed. A peer's tick can take the closing CAS,
/// which is why it retries; only a CPU that closed an epoch can take one from
/// here, so one attempt per CPU is enough.
#[cfg(feature = "test-hooks")]
pub fn force_close_epoch_for_test() -> Option<u64> {
    for _ in 0..MAX_CPUS {
        let epoch = EPOCH.load(Ordering::Acquire);
        for cpu in 0..MAX_CPUS {
            if slopos_arch::pcr::is_cpu_online(cpu) {
                CPU_ACKED[cpu].fetch_max(epoch, Ordering::AcqRel);
            }
        }
        if try_close(epoch) {
            return Some(epoch);
        }
    }
    None
}

/// Diagnostics: `(epoch, advance_requested, last_deferred_epoch)`.
pub fn stats() -> (u64, bool, u64) {
    (
        EPOCH.load(Ordering::Relaxed),
        ADVANCE_REQUESTED.load(Ordering::Relaxed),
        LAST_DEFERRED_EPOCH.load(Ordering::Relaxed),
    )
}
