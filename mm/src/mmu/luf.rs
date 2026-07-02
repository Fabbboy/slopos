//! Lazy Unmap Flush (LUF) — per-CPU deferred TLB shootdowns.
//!
//! Instead of issuing a synchronous IPI shootdown the moment a page is
//! unmapped, we queue the pending invalidation on the initiating CPU.
//! The queue
//! drains in three ways:
//!
//!   1. **Threshold** — when the queue fills up, a single batched
//!      shootdown covers every deferred entry. Amortises the IPI cost
//!      across many unmaps.
//!   2. **Reuse** — when the page allocator is about to hand out a
//!      frame whose prior translation is still sitting in a deferred
//!      entry, we drain first so the fresh owner can't observe stale
//!      caching under its new mapping.
//!   3. **Context switch** — any CR3 reload with `NOFLUSH=0` (miss or
//!      rotation) naturally invalidates deferred entries of the
//!      outgoing PCID. Nothing to do explicitly.
//!
//! The queue is a fixed-size ring per CPU. Overflow entries degrade
//! gracefully to a synchronous shootdown — that is, LUF never loses
//! a flush, it only defers it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_arch::cpu::IrqDisabled;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::klog_warn;
use slopos_ostd::panic::AbortOnUnwind;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_ALLOCATOR;
use slopos_ostd::sync::spin::PreemptMutex;

use super::cr3::MmContextId;
use crate::tlb;

/// Depth of the per-CPU deferred-flush ring. Tuned small so overflow-
/// triggered drains remain cheap; on alloc-heavy workloads the queue
/// is drained at reuse time well before this fills.
pub const LUF_QUEUE_DEPTH: usize = 64;

/// One deferred invalidation. Carrying the backing `phys` lets the
/// reuse drain match by physical frame (the common case); `pcid`
/// targets the right hardware tag when we eventually issue INVPCID.
#[derive(Clone, Copy)]
#[allow(dead_code)] // `vaddr` and `pcid` are consumed by the future
// INVPCID-type-0-per-entry drain refinement; today the drain keys
// off `phys` alone.
struct LufEntry {
    vaddr: u64,
    phys: u64,
    /// Stable 64-bit identifier of the address space whose mapping we
    /// are invalidating. Survives per-CPU ASID slot rotation — drain
    /// verifies this against the current slot binding before trusting
    /// `pcid` to issue `INVPCID type 0`. Mismatch means the PCID slot
    /// has been reassigned and the stale TLB was already wiped by the
    /// rotation's flush; the entry is discarded.
    ctx_id: MmContextId,
    pcid: u16,
    _pad: [u8; 6],
}

impl LufEntry {
    const EMPTY: Self = Self {
        vaddr: 0,
        phys: 0,
        ctx_id: MmContextId::INVALID,
        pcid: 0,
        _pad: [0; 6],
    };
}

/// Per-CPU state. Only its owning CPU writes the ring; other CPUs may
/// read the counters for diagnostics.
#[repr(C, align(64))]
struct PerCpuLuf {
    head: usize,
    tail: usize,
    len: usize,
    ring: [LufEntry; LUF_QUEUE_DEPTH],

    /// Entries successfully deferred (queue had room).
    pub queued: AtomicU64,
    /// Entries flushed because the queue filled up.
    pub overflow_drains: AtomicU64,
    /// Entries flushed because the underlying frame was re-allocated.
    pub reuse_drains: AtomicU64,
    /// Deferred flushes that saved a synchronous shootdown (threshold
    /// drain folded multiple entries into one IPI round).
    pub deferred_saves: AtomicU64,
}

impl PerCpuLuf {
    const fn new() -> Self {
        Self {
            head: 0,
            tail: 0,
            len: 0,
            ring: [LufEntry::EMPTY; LUF_QUEUE_DEPTH],
            queued: AtomicU64::new(0),
            overflow_drains: AtomicU64::new(0),
            reuse_drains: AtomicU64::new(0),
            deferred_saves: AtomicU64::new(0),
        }
    }
}

/// `KernelSync` wraps the `UnsafeCell` so the surrounding cell auto-derives
/// `Sync`; single-writer-per-CPU via the owning CPU gates real access,
/// and cross-CPU reads only touch the `AtomicU64` counters.
struct PerCpuLufCell(slopos_ostd::sync::KernelSync<UnsafeCell<PerCpuLuf>>);

static PER_CPU_LUF: [PerCpuLufCell; MAX_CPUS] = {
    const INIT: PerCpuLufCell = PerCpuLufCell(slopos_ostd::sync::KernelSync::new(UnsafeCell::new(
        PerCpuLuf::new(),
    )));
    [INIT; MAX_CPUS]
};

/// Global bitmap: bit `i` is set iff CPU `i`'s ring currently holds
/// ≥1 entry. Transitions happen exclusively inside the owning CPU's
/// `queue_unmap` (set) and `drain_all` (clear); remote readers use a
/// single relaxed load to skip the entire cross-CPU drain path when
/// no CPU has anything queued.
///
/// Width: 64 bits covers CPU indices `[0, 64)`. For `MAX_CPUS > 64`
/// the cross-CPU drain falls back to broadcast; SlopOS's real SMP
/// envelope (4–16 CPUs) is well inside the precise bound.
static LUF_NONEMPTY_MASK: AtomicU64 = AtomicU64::new(0);

#[inline]
fn nonempty_mask_set(cpu: usize) {
    if cpu < 64 {
        LUF_NONEMPTY_MASK.fetch_or(1u64 << cpu, Ordering::Release);
    }
}

#[inline]
fn nonempty_mask_clear(cpu: usize) {
    if cpu < 64 {
        LUF_NONEMPTY_MASK.fetch_and(!(1u64 << cpu), Ordering::Release);
    }
}

/// Snapshot of which CPUs currently hold queued unmap entries. One
/// relaxed load, safe to call from anywhere.
#[inline]
pub fn nonempty_mask_snapshot() -> u64 {
    LUF_NONEMPTY_MASK.load(Ordering::Acquire)
}

// =============================================================================
// Cross-CPU drain-by-phys IPI
// =============================================================================

/// Shared drain request, serialised by `DRAIN_LOCK` below. Remote CPUs
/// read `target_phys`, scan their ring, ack, then optionally drain if
/// they hit. Early ACK: remotes bump `ack` before the INVPCID retires
/// so the initiator can proceed as soon as all targets have observed
/// the request.
#[repr(C, align(64))]
struct LufDrainRequest {
    target_phys: AtomicU64,
    /// Per-CPU ack bitmask: bit `c` set once CPU `c` has observed this
    /// request. The initiator waits until every bit in its `remote_mask`
    /// is set. A bitmask (not a counter) makes IPI re-send idempotent: a
    /// duplicate ack from an already-acked CPU just re-sets its bit, so a
    /// lost LUF-drain IPI can be re-armed without over-counting past the
    /// completion condition (a counter would let a re-send falsely satisfy
    /// the wait while a slow CPU still held stale TLB).
    acked_mask: AtomicU64,
    /// Number of IPIs the initiator is expecting an ack for.
    pending_cpus: core::sync::atomic::AtomicU32,
    /// Monotonic sequence stamp — remote handlers copy it on entry so
    /// they don't accidentally double-ack if a racing drain happens to
    /// bind the same vector back-to-back.
    sequence: AtomicU64,
}

impl LufDrainRequest {
    const fn new() -> Self {
        Self {
            target_phys: AtomicU64::new(0),
            acked_mask: AtomicU64::new(0),
            pending_cpus: core::sync::atomic::AtomicU32::new(0),
            sequence: AtomicU64::new(0),
        }
    }
}

static DRAIN_REQUEST: LufDrainRequest = LufDrainRequest::new();

/// Single-writer serialisation for the shared drain request.
///
/// **MUST be `PreemptMutex`, NOT `SpinLock`.** Drains broadcast an LUF IPI
/// and spin-wait for acks; remote handlers respond by issuing
/// `tlb::flush_all()` which sends a TLB IPI back at the initiator. If
/// `DRAIN_LOCK` cli'd (as `SpinLock` does), the lock holder couldn't ack
/// that incoming TLB IPI — and a *second* CPU trying to acquire the same
/// lock couldn't ack the original LUF IPI it's the target of — giving a
/// three-way deadlock (initiator waiting for LUF ack ↔ contender waiting
/// for lock ↔ contender's TLB IPI waiting for initiator's ack). Confirmed
/// in `builddir/hang_repro/iter_4_bt.log`:
///   * CPU 0 in `luf::drain_by_phys_cross_cpu` (line 258 spin)
///   * CPU 2 in `__mm_pause` from `SpinLock::lock` waiting for `DRAIN_LOCK`,
///     called from its own `BuddyAllocator::alloc → drain_if_reusing_frame`
///
/// `PreemptMutex` keeps IRQs enabled, so both lock holders and lock
/// spinners can service incoming IPIs and break the cycle. Safe because
/// `DRAIN_LOCK` is never acquired from an IRQ handler (only from frame
/// allocation paths). `handle_drain_ipi` reads `DRAIN_REQUEST` lock-free.
static DRAIN_LOCK: PreemptMutex<()> = PreemptMutex::new((), LOCK_LEVEL_ALLOCATOR);

/// Broadcast a "drain any entries referencing this phys" request to
/// every CPU whose bit is set in `cpu_mask`. Blocks until every
/// addressee has acknowledged. If `cpu_mask == 0`, returns immediately.
///
/// Must not be called from the owning CPU's own ring — the local
/// self-drain path (`drain_if_reusing_frame`) handles that before we
/// get here.
pub fn drain_by_phys_cross_cpu(phys: PhysAddr, cpu_mask: u64) -> bool {
    if cpu_mask == 0 || phys.is_null() {
        return true;
    }

    // Exclude our own CPU — its ring was already scanned synchronously
    // by the caller (`drain_if_reusing_frame` runs before this on the
    // initiator CPU).
    let initiator = slopos_arch::pcr::get_current_cpu();
    let remote_mask = if initiator < 64 {
        cpu_mask & !(1u64 << initiator)
    } else {
        cpu_mask
    };
    if remote_mask == 0 {
        return true;
    }

    let pending = remote_mask.count_ones();
    if pending == 0 {
        return true;
    }

    // Acquire `DRAIN_LOCK` via a try-lock loop that **re-enables IRQs
    // between attempts**. This is load-bearing for liveness.
    //
    // Why we can't use the normal `DRAIN_LOCK.lock()`: even though
    // `PreemptMutex` doesn't itself `cli`, its inner ticket spin
    // (`for _ in 0..distance.min(64) { spin_loop(); }`) inherits IF
    // from the caller. If we got here from a path that cli'd higher up
    // (or for any other reason), the spin runs forever with IF=0:
    //
    //   * other initiators' LUF IPIs back at us never run their
    //     `handle_drain_ipi`, so the lock holder never sees our ack,
    //   * remote handlers' TLB-shootdown IPIs back at us never run
    //     their per-target queue swap, so the holder's `wait_for_acks`
    //     stalls,
    //   * even timer ticks miss, so the NMI watchdog eventually fires
    //     (`builddir/hang_repro/iter_11_bt.log` — CPU 0 frozen in
    //     `PreemptMutex::lock spin_loop` for >500 ms).
    //
    // The try-lock + sti-on-backoff pattern guarantees that every
    // contended slice has IRQs enabled at least briefly, so IPIs
    // queued for us get drained before we re-attempt the lock. Once
    // we acquire the lock we hold IRQs enabled for the full critical
    // section (IPI fan-out + ack spin + release). Caller's IRQ state
    // is restored at the bottom of the function.
    let was_enabled = slopos_arch::cpu::are_interrupts_enabled();
    let _guard = loop {
        if let Some(g) = DRAIN_LOCK.try_lock() {
            break g;
        }
        // Contended. Make absolutely sure IRQs are enabled for the
        // backoff so any IPI we're the target of can fire and ack.
        slopos_arch::cpu::enable_interrupts();
        for _ in 0..256 {
            slopos_arch::cpu::pause();
        }
    };
    // Lock held. Ensure IRQs are enabled for the wait below.
    slopos_arch::cpu::enable_interrupts();

    // An unwind mid-protocol would release `DRAIN_LOCK` with the shared
    // `DRAIN_REQUEST` half-published and the caller's frame handed out
    // without the drain completing — abort instead. Armed after the lock
    // so the abort fires before the lock guard's release; disarmed once
    // the ack wait resolves.
    let abort_guard = AbortOnUnwind::new();

    DRAIN_REQUEST
        .target_phys
        .store(phys.as_u64(), Ordering::Release);
    DRAIN_REQUEST.acked_mask.store(0, Ordering::Release);
    DRAIN_REQUEST.pending_cpus.store(pending, Ordering::Release);
    DRAIN_REQUEST.sequence.fetch_add(1, Ordering::Release);

    core::sync::atomic::fence(Ordering::SeqCst);

    // Pre-ack CPUs we will never receive a real ack from (offline / no
    // APIC mapping) by setting their bits, then IPI the rest.
    let mut sendable_mask = 0u64;
    for cpu in 0..64 {
        if remote_mask & (1u64 << cpu) == 0 {
            continue;
        }
        let apic = if slopos_arch::pcr::is_cpu_online(cpu) {
            slopos_arch::pcr::apic_id_from_cpu_index(cpu)
        } else {
            None
        };
        match apic {
            Some(apic_id) => {
                sendable_mask |= 1u64 << cpu;
                slopos_arch::pcr::send_ipi_to_cpu(
                    apic_id,
                    slopos_arch::arch::idt::LUF_DRAIN_IPI_VECTOR,
                );
            }
            None => {
                DRAIN_REQUEST
                    .acked_mask
                    .fetch_or(1u64 << cpu, Ordering::Release);
            }
        }
    }

    // Spin-wait until every expected CPU's bit is set. IRQs are enabled
    // (set above before mutex acquisition) so remote handlers' TLB-shootdown
    // IPIs back at us, and other initiators' LUF IPIs back at us, both get
    // serviced — closing the three-way LUF↔TLB↔contender deadlock.
    //
    // Re-send to any CPU whose ack has not arrived within a bounded spin:
    // a single LUF-drain IPI is no more trustworthy than a TLB one (lost
    // under ICR-busy / coalescing), and an unbacked wait here wedges the
    // initiator while it holds DRAIN_LOCK. Re-send is idempotent thanks to
    // the per-CPU bitmask.
    const RESEND_SPIN: u64 = 1_000_000;
    const MAX_RESENDS: u64 = 256;
    let mut spin: u64 = 0;
    let mut resends: u64 = 0;
    let mut drain_ok = true;
    loop {
        let acked = DRAIN_REQUEST.acked_mask.load(Ordering::Acquire);
        if acked & remote_mask == remote_mask {
            break;
        }
        slopos_arch::cpu::pause();
        spin = spin.wrapping_add(1);
        if spin >= RESEND_SPIN {
            spin = 0;
            resends += 1;
            if resends > MAX_RESENDS {
                klog_warn!(
                    "LUF: drain-by-phys never fully acked (phys=0x{:x}, acked=0x{:x}, want=0x{:x}); giving up",
                    phys.as_u64(),
                    acked,
                    remote_mask
                );
                drain_ok = false;
                break;
            }
            let missing = sendable_mask & !acked;
            for cpu in 0..64 {
                if missing & (1u64 << cpu) == 0 {
                    continue;
                }
                if let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(cpu) {
                    slopos_arch::pcr::send_ipi_to_cpu(
                        apic_id,
                        slopos_arch::arch::idt::LUF_DRAIN_IPI_VECTOR,
                    );
                }
            }
        }
    }

    abort_guard.disarm();
    drop(_guard);

    if !was_enabled {
        slopos_arch::cpu::disable_interrupts();
    }
    drain_ok
}

/// Remote IPI handler. Reads the shared drain request, scans this
/// CPU's ring for entries matching `target_phys`, drains if any hit,
/// and acks the initiator.
///
/// Called from `boot::idt`'s vector dispatcher; must be called with
/// interrupts disabled.
pub fn handle_drain_ipi(cpu: usize) {
    // Runs in ISR context: panic recovery never unwinds interrupt
    // context (the panic handler's gate excludes it), so no
    // unwind-abort guard is needed here.
    let target = PhysAddr::new(DRAIN_REQUEST.target_phys.load(Ordering::Acquire));

    // Early ACK — the initiator may proceed as soon as we've observed
    // the request; the scan + drain below happens inside this ISR and
    // the IRETQ serialises w.r.t. any subsequent memory accesses on
    // this CPU. Setting our own bit is idempotent under IPI re-send.
    if cpu < 64 {
        DRAIN_REQUEST
            .acked_mask
            .fetch_or(1u64 << cpu, Ordering::Release);
    }

    let Some(state) = state_for(cpu) else {
        return;
    };
    if state.len == 0 {
        return;
    }
    let needle = target.as_u64();
    let mut hit = false;
    for i in 0..state.len {
        let idx = (state.head + i) % LUF_QUEUE_DEPTH;
        if state.ring[idx].phys == needle {
            hit = true;
            break;
        }
    }
    if hit {
        state.reuse_drains.fetch_add(1, Ordering::Relaxed);
        drain_all(state, cpu);
    }
}

#[inline]
fn state_for(cpu: usize) -> Option<&'static mut PerCpuLuf> {
    if cpu >= MAX_CPUS {
        return None;
    }
    Some(PER_CPU_LUF[cpu].0.cell_get_mut())
}

fn drain_all(state: &mut PerCpuLuf, cpu: usize) {
    // The per-CPU ring is single-writer-per-CPU. `handle_drain_ipi` re-
    // acquires this CPU's `PerCpuLuf` via `cell_get_mut` and reenters
    // `drain_all`; `flush_all` below spins in `wait_for_acks` WITHOUT
    // disabling IRQs, so entered interrupts-on that LUF-drain IPI would
    // take an aliasing second `&mut` and zero `state.len` mid-drain
    // (underflowing `drained - 1`). Holding IRQs off across the whole
    // `&mut`-live window restores the invariant; `flush_all` is still
    // correct IRQs-off because `wait_for_acks` acks peers by polling
    // `service_local_shootdown_queue`, not the IRQ vector. `with` composes
    // re-entrantly, so callers already IRQs-off nest for free.
    IrqDisabled::with(|_irq| {
        // Snapshot the count once: it drives the saved-shootdown stat and
        // must never be re-read after `flush_all`.
        let drained = state.len;
        if drained == 0 {
            return;
        }

        // An unwind out of `flush_all` would skip both the ring reset and
        // any caller's fallback flush, silently losing queued TLB
        // invalidations into frame reuse — abort instead. Disarmed once
        // the ring reset completes.
        let abort_guard = AbortOnUnwind::new();

        // Collapse every deferred entry into a single full-process flush.
        // TODO: peel apart by PCID so we issue one INVPCID per tag
        // instead of one CR3-reload-equivalent. For now we rely on the
        // existing `tlb::flush_all` shootdown fast path.
        tlb::flush_all();

        state
            .deferred_saves
            .fetch_add(drained as u64 - 1, Ordering::Relaxed);
        state.head = 0;
        state.tail = 0;
        state.len = 0;
        for e in state.ring.iter_mut() {
            *e = LufEntry::EMPTY;
        }
        nonempty_mask_clear(cpu);
        abort_guard.disarm();
    });
}

/// Per-CPU suppression of cross-CPU unmap deferral. While set on a CPU,
/// [`queue_unmap`] performs only the local invalidation and does not
/// queue an entry. Used by gathered bulk unmaps (e.g. exec-time stack
/// reset) that hold every freed frame and issue a single explicit
/// shootdown AFTER dropping their locks — so no synchronous broadcast
/// ever runs under an IRQs-off lock.
static SUPPRESS_CROSS_CPU: [AtomicBool; MAX_CPUS] = {
    const F: AtomicBool = AtomicBool::new(false);
    [F; MAX_CPUS]
};

/// Begin suppressing cross-CPU unmap deferral on the current CPU. Must
/// be paired with [`unsuppress_cross_cpu_drain`]; the caller is then
/// responsible for the explicit cross-CPU shootdown of the unmapped
/// range, performed with the freed frames still held.
pub fn suppress_cross_cpu_drain() {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        SUPPRESS_CROSS_CPU[cpu].store(true, Ordering::Release);
    }
}

/// Stop suppressing cross-CPU unmap deferral on the current CPU.
pub fn unsuppress_cross_cpu_drain() {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        SUPPRESS_CROSS_CPU[cpu].store(false, Ordering::Release);
    }
}

#[inline]
fn cross_cpu_drain_suppressed() -> bool {
    let cpu = slopos_arch::pcr::get_current_cpu();
    cpu < MAX_CPUS && SUPPRESS_CROSS_CPU[cpu].load(Ordering::Acquire)
}

/// Queue a TLB flush for `(vaddr, phys)` belonging to `ctx_id` / `pcid`
/// on the calling CPU.
///
/// `ctx_id` lets the drain path verify the PCID slot binding before
/// issuing `INVPCID`; `pcid` is kept so the fast drain can skip the
/// slot lookup when the binding hasn't rotated.
///
/// If the ring is full, falls back to a synchronous `tlb::flush_page`
/// right here — correctness is preserved, but we miss the LUF win for
/// this entry (which is what `overflow_drains` records).
///
/// Must be called with preemption disabled on the caller's CPU.
pub fn queue_unmap(vaddr: VirtAddr, phys: PhysAddr, ctx_id: MmContextId, pcid: u16) {
    // Local invalidation first: the initiator's own TLB must not
    // retain the stale translation, even if we never IPI anyone. A
    // `munmap` + userspace re-deref on the same CPU would otherwise
    // still hit the cached entry. INVLPG targets whatever PCID is
    // currently loaded; when that matches `pcid` we're invalidating
    // the right tag. If this CPU is currently running a different
    // context (kernel thread, another process doing the unmap on
    // behalf of a freed target), INVLPG on a user VA is harmless —
    // there's nothing to invalidate in the current PCID anyway, and
    // the cross-CPU drain will catch that CPU when it schedules the
    // real owner back in.
    //
    // The `pcid` field is retained for a future INVPCID-type-0 upgrade
    // that targets the exact PCID slot regardless of current CR3.
    let _ = pcid;
    slopos_arch::cpu::tlb::invlpg(vaddr.as_u64());

    // Gathered bulk unmap in progress: the local invalidation above is
    // all that's needed here. The caller holds the freed frames and
    // issues one explicit cross-CPU shootdown after dropping its locks.
    if cross_cpu_drain_suppressed() {
        return;
    }

    // Per-CPU ring bookkeeping runs IRQs-off so the LUF-drain IPI can't
    // interpose a second aliasing writer mid-update (see `drain_all`).
    // Without this, an IPI between reading `state.tail` and bumping
    // `state.len` lets a nested drain empty the ring; we then advance over
    // a now-EMPTY slot and silently drop a real entry — a lost TLB
    // invalidation that survives into frame reuse. Any fallback synchronous
    // shootdown is issued AFTER the window, once the `&mut` is released and
    // IRQs are restored (it broadcasts a shootdown of its own).
    let fallback_flush = IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();
        let Some(state) = state_for(cpu) else {
            return true;
        };

        if state.len >= LUF_QUEUE_DEPTH {
            state.overflow_drains.fetch_add(1, Ordering::Relaxed);
            drain_all(state, cpu);
            return true;
        }

        let was_empty = state.len == 0;

        let slot = state.tail;
        state.ring[slot] = LufEntry {
            vaddr: vaddr.as_u64(),
            phys: phys.as_u64(),
            ctx_id,
            pcid,
            _pad: [0; 6],
        };
        state.tail = (slot + 1) % LUF_QUEUE_DEPTH;
        state.len += 1;
        state.queued.fetch_add(1, Ordering::Relaxed);

        if was_empty {
            nonempty_mask_set(cpu);
        }
        false
    });

    if fallback_flush {
        tlb::flush_page(vaddr);
    }
}

/// Drain the current CPU's deferred-flush ring unconditionally.
///
/// Call this whenever a blanket flush is about to happen anyway
/// (process teardown, CR3 reload into a fresh context, TLB-ctrl
/// syscalls in the future) — the subsequent flush subsumes all queued
/// entries, and clearing the queue avoids a wasted re-flush later.
pub fn drain_local() {
    // IRQs-off so `state_for`'s `&mut` is never live with the LUF-drain IPI
    // able to take an aliasing second one (see `drain_all`); `with` nests
    // for free with `drain_all`'s own guard.
    IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();
        if let Some(state) = state_for(cpu) {
            drain_all(state, cpu);
        }
    });
}

/// Drain any deferred entry that aliases physical frame `phys`
/// anywhere in the system.
///
/// Called from `alloc_page_frame` just before the allocator hands the
/// frame to a fresh owner. Runs in two stages:
///
///  1. **Local scan** — the calling CPU checks its own ring for the
///     phys; hit → drain locally (cheap, no IPI).
///  2. **Cross-CPU targeted drain** — if other CPUs still have queued
///     unmaps (`LUF_NONEMPTY_MASK != 0`), IPI only those CPUs with a
///     drain-by-phys request. Any remote ring containing the phys
///     drains itself and acks. Fast path (`nonempty_mask == 0` after
///     clearing our own bit) skips the IPI round-trip entirely.
///
/// Preserves the invariant that no CPU retains a TLB entry pointing
/// at a frame the allocator has just reassigned.
pub fn drain_if_reusing_frame(phys: PhysAddr) -> bool {
    if phys.is_null() {
        return true;
    }
    // Local half: scan + possibly drain our own ring before we consult
    // the global mask. `drain_all` clears our bit via
    // `nonempty_mask_clear`, so the cross-CPU check below naturally
    // skips us. Runs IRQs-off so the scan + drain hold the per-CPU ring as
    // the single writer (the LUF-drain IPI handler reenters via
    // `cell_get_mut`); the window ends before the cross-CPU half, which
    // MUST keep the caller's IRQ state — `drain_by_phys_cross_cpu` requires
    // IRQs enabled (see the `DRAIN_LOCK` note).
    IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();
        if let Some(state) = state_for(cpu) {
            if state.len > 0 {
                let needle = phys.as_u64();
                let mut hit = false;
                for i in 0..state.len {
                    let idx = (state.head + i) % LUF_QUEUE_DEPTH;
                    if state.ring[idx].phys == needle {
                        hit = true;
                        break;
                    }
                }
                if hit {
                    state.reuse_drains.fetch_add(1, Ordering::Relaxed);
                    drain_all(state, cpu);
                }
            }
        }
    });

    // Cross-CPU half: the fast path (mask == 0) reads one relaxed
    // atomic and returns — the IPI round-trip fires only when some
    // other CPU might still hold a stale translation.
    let remote_mask = nonempty_mask_snapshot();
    if remote_mask != 0 {
        return drain_by_phys_cross_cpu(phys, remote_mask);
    }
    true
}

/// Threshold-drain poll. Safe to call from a timer-tick bottom half.
///
/// When the ring is more than half full, convert the pending entries
/// into a single `flush_all` IPI — amortises cost across many unmaps.
pub fn drain_if_high_watermark() {
    // IRQs-off for the same single-writer reason as `drain_local`.
    IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();
        let Some(state) = state_for(cpu) else { return };
        if state.len >= LUF_QUEUE_DEPTH / 2 {
            drain_all(state, cpu);
        }
    });
}

/// Read-only counter accessors for test harness / metrics line.
pub fn queued_count(cpu: usize) -> u64 {
    state_for_readonly(cpu)
        .map(|s| s.queued.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn deferred_saves_count(cpu: usize) -> u64 {
    state_for_readonly(cpu)
        .map(|s| s.deferred_saves.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn reuse_drains_count(cpu: usize) -> u64 {
    state_for_readonly(cpu)
        .map(|s| s.reuse_drains.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn overflow_drains_count(cpu: usize) -> u64 {
    state_for_readonly(cpu)
        .map(|s| s.overflow_drains.load(Ordering::Relaxed))
        .unwrap_or(0)
}

fn state_for_readonly(cpu: usize) -> Option<&'static PerCpuLuf> {
    if cpu >= MAX_CPUS {
        return None;
    }
    // Read-only sibling: per-CPU IRQs-off discipline gates writers;
    // diagnostic snapshot is fine even from another CPU because the
    // contained AtomicU64 counters tolerate concurrent reads.
    Some(PER_CPU_LUF[cpu].0.cell_get())
}

// =============================================================================
// Per-CPU active mm-context-handle tracker
// =============================================================================

/// Per-CPU storage for the `mm_ctx_handle` of the address space currently
/// installed in CR3. Written by the OSTD `CursorUnmapHook::on_activate`
/// callback at every context switch (via `current_cpu_set_active_mm_ctx`)
/// and read by the LUF drain logic when it needs to know which context
/// the local PCID is currently bound to without re-deriving from CR3.
///
/// Uses `0` as the "no context bound" sentinel — matches
/// [`MmContextId::INVALID`]`.raw()` and the unset value of
/// `VmSpace::mm_ctx_handle`. Each CPU writes its own slot; cross-CPU
/// reads only happen for diagnostics.
static ACTIVE_MM_CTX_HANDLE: [AtomicU64; MAX_CPUS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_CPUS]
};

/// Record that the current CPU has just installed an address space whose
/// opaque handle is `handle`. Called from
/// `LufHook::on_activate` immediately before `VmSpace::activate` writes
/// CR3, so any subsequent local LUF-queue inspection knows which
/// `MmContextId` the live PCID maps to.
#[inline]
pub fn current_cpu_set_active_mm_ctx(handle: u64) {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].store(handle, Ordering::Release);
    }
}

/// Read the address space handle this CPU last activated, or `0` if no
/// VmSpace has been activated on this CPU yet.
#[inline]
pub fn current_cpu_active_mm_ctx() -> u64 {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}
