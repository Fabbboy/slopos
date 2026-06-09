//! TLB (Translation Lookaside Buffer) Shootdown Implementation
//!
//! This module provides cross-CPU TLB invalidation for SMP systems.
//! When page table entries are modified (unmap, permission change, etc.),
//! we must ensure all CPUs invalidate their cached translations.
//!
//! # Architecture
//!
//! On uniprocessor systems, a simple `invlpg` instruction suffices.
//! On SMP systems, we must:
//! 1. Invalidate on the local CPU
//! 2. Send IPIs to all other CPUs
//! 3. Wait for acknowledgment before returning
//!
//! # Optimizations
//!
//! - INVPCID instruction support for more efficient invalidation
//! - Batched flushes to reduce IPI overhead
//! - Full CR3 reload for large ranges (cheaper than many invlpg)
//! - Per-address-space invalidation via PCID (when available)

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_arch::cpu;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};
use slopos_ostd::{klog_debug, klog_info, klog_warn};

use crate::memory_layout_defs::MAX_PROCESSES;
use crate::paging_defs::PAGE_SIZE_4KB;

/// Function pointer type for sending TLB shootdown IPI.
/// Called with the IPI vector number.
pub type SendIpiFn = fn(u8);

/// Registered IPI sender function (set by drivers/apic during init).
static IPI_SENDER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Register the IPI sender function.
/// Must be called from the APIC driver during initialization.
pub fn register_ipi_sender(sender: SendIpiFn) {
    IPI_SENDER.store(sender as *mut (), Ordering::Release);
    klog_debug!("TLB: IPI sender registered");
}

// =============================================================================
// Configuration Constants
// =============================================================================

/// Maximum number of pages to invalidate individually before switching to full flush.
/// Beyond this threshold, a full TLB flush (CR3 reload) is cheaper.
const INVLPG_THRESHOLD: usize = 32;

pub use slopos_arch::arch::idt::TLB_SHOOTDOWN_VECTOR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbShootdownTimeout {
    pub cpu_idx: usize,
    pub resends: u64,
}

type TlbResult = Result<(), TlbShootdownTimeout>;

// =============================================================================
// CPU Feature Detection
// =============================================================================

use slopos_arch::cpu::cpuid::{
    CPUID_FEAT_ECX_PCID, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_INVPCID,
};

/// Cached CPU feature flags for TLB operations.
struct TlbFeatures {
    /// CPU supports INVPCID instruction.
    invpcid_supported: AtomicBool,
    /// CPU supports PCID (CR4.PCIDE).
    pcid_supported: AtomicBool,
    /// Features have been detected.
    initialized: AtomicBool,
}

static TLB_FEATURES: TlbFeatures = TlbFeatures {
    invpcid_supported: AtomicBool::new(false),
    pcid_supported: AtomicBool::new(false),
    initialized: AtomicBool::new(false),
};

fn detect_features() {
    if TLB_FEATURES.initialized.load(Ordering::Acquire) {
        return;
    }

    let (_, _, ecx, _) = cpu::cpuid(CPUID_LEAF_FEATURES);
    let pcid_supported = (ecx & CPUID_FEAT_ECX_PCID) != 0;

    let (max_leaf, _, _, _) = cpu::cpuid(0);
    let invpcid_supported = if max_leaf >= CPUID_LEAF_STRUCTURED_EXT {
        let (_, ebx, _, _) = cpu::cpuid(CPUID_LEAF_STRUCTURED_EXT);
        (ebx & CPUID_SEXT_EBX_INVPCID) != 0
    } else {
        false
    };

    TLB_FEATURES
        .pcid_supported
        .store(pcid_supported, Ordering::Release);
    TLB_FEATURES
        .invpcid_supported
        .store(invpcid_supported, Ordering::Release);
    TLB_FEATURES.initialized.store(true, Ordering::Release);

    klog_debug!(
        "TLB: Features detected - PCID: {}, INVPCID: {}",
        pcid_supported,
        invpcid_supported
    );
}

/// Check if INVPCID instruction is available.
#[inline]
pub fn has_invpcid() -> bool {
    if !TLB_FEATURES.initialized.load(Ordering::Acquire) {
        detect_features();
    }
    TLB_FEATURES.invpcid_supported.load(Ordering::Relaxed)
}

/// Check if PCID is available.
#[inline]
pub fn has_pcid() -> bool {
    if !TLB_FEATURES.initialized.load(Ordering::Acquire) {
        detect_features();
    }
    TLB_FEATURES.pcid_supported.load(Ordering::Relaxed)
}

// =============================================================================
// SMP State Tracking
// =============================================================================

// =============================================================================
// Per-target slot: lock-protected pending op + ack flag.
//
// Design (cf. Asterinas `ostd/src/mm/tlb.rs`, Redox `src/percpu.rs`):
//
// Each target CPU has a slot containing a `SpinLock<PendingTlbReq>` and an
// `AtomicBool` ack. The shootdown protocol holds the lock across BOTH the
// initiator's "push request + clear ACK" and the handler's "take request +
// set ACK" critical sections. That serialisation is the entire trick.
//
// Why the previous design hung:
//
//   1. CPU 0 sets cpu_state[2].ack=false, sends IPI to 2
//   2. CPU 1 concurrently sets cpu_state[2].ack=false (still false), sends IPI
//   3. LAPIC coalesces the two IPIs into one delivery on CPU 2
//   4. CPU 2's single handler invocation does `ack=true`
//   5. CPU 1 wins the read race, sees ack=true, **stores ack=false** and exits
//   6. CPU 0 reads ack=false; spins forever — no further IPI will ever arrive
//
// The new protocol's invariants:
//
//   * `ack` transitions to `false` are written ONLY by `queue_request_for_cpu`,
//     which holds `queue.lock` for the entire push+clear sequence.
//   * `ack` transitions to `true` are written ONLY by `handle_shootdown_ipi`,
//     which holds the same lock for the entire take+set sequence.
//   * `wait_for_acks` is a pure reader (Acquire load); it does NOT reset ack.
//
// So two initiators interleave at the lock; the handler's invocation drains
// the merged queue and stamps `ack=true`; both initiators' subsequent reads
// observe the same `true`. A coalesced IPI delivers ONE handler invocation,
// but that one invocation has the merged work for ALL initiators, so per-
// initiator visibility is preserved.
// =============================================================================

/// Pending TLB-flush request in a per-target slot.
///
/// We merge concurrent requests in place instead of queueing them. If the
/// slot is already non-empty when a second initiator pushes, the merged
/// result is promoted to `Full` — coarser than the asterinas per-op stack
/// (their threshold for full-flush promotion is 32 ops; ours is 2) but
/// trivially correct under contention. Single-op pushes preserve their
/// shape, so the common case is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingFlush {
    /// Slot is empty.
    None,
    /// Flush exactly one page at `start`. Kept separate from `Range`
    /// because the highest user page (`start = 0x7FFFFFFFFFFFF000`) would
    /// produce `end = 0x8000000000000000`, which is non-canonical on
    /// x86-64 and panics `VirtAddr::new`. Storing only `start` sidesteps
    /// that without needing a non-panicking VirtAddr variant.
    SinglePage { start: u64 },
    /// Flush every page in `[start, end)`. Caller guarantees `start < end`
    /// and `end` is canonical.
    Range { start: u64, end: u64 },
    /// Flush all TLB entries (CR3 reload).
    Full,
}

/// `op` paired with the ASID it targets. `asid == 0` means "any address
/// space" (the handler always flushes); a non-zero `asid` means the handler
/// only flushes if its local CR3 currently matches `asid & !0xFFF`.
struct PendingTlbReq {
    op: PendingFlush,
    asid: u64,
}

impl PendingTlbReq {
    const fn new() -> Self {
        Self {
            op: PendingFlush::None,
            asid: 0,
        }
    }

    /// Merge a fresh request into the slot. Single-op stays as-is; any
    /// second push promotes to `Full`, and mismatched ASIDs widen to 0.
    fn push(&mut self, op: PendingFlush, asid: u64) {
        match self.op {
            PendingFlush::None => {
                self.op = op;
                self.asid = asid;
            }
            _ => {
                self.op = PendingFlush::Full;
                if self.asid != asid {
                    self.asid = 0;
                }
            }
        }
    }

    /// Empty the slot, returning the merged op + asid.
    fn take(&mut self) -> (PendingFlush, u64) {
        let op = core::mem::replace(&mut self.op, PendingFlush::None);
        let asid = core::mem::replace(&mut self.asid, 0);
        (op, asid)
    }
}

/// Per-CPU TLB shootdown slot. One per target CPU.
///
/// `queue` carries the merged pending request, written only under the lock.
/// `ack` is the per-target completion flag — handler stores `true` under the
/// queue lock; initiator clears `false` under the queue lock; `wait_for_acks`
/// reads (Acquire) and never writes. See the file-level comment for the race
/// this protocol closes.
#[repr(C, align(64))] // Cache line aligned to prevent false sharing
struct PerCpuTlbState {
    /// Lock-protected pending request (see `PendingTlbReq`).
    queue: SpinLock<PendingTlbReq>,
    /// Per-target ack flag. Set `true` by handler under `queue` lock after
    /// `take`; cleared `false` by initiator under `queue` lock after `push`.
    /// `wait_for_acks` is a pure reader and MUST NOT write this.
    ack: AtomicBool,
    /// Lazy-TLB optimisation (orthogonal to the ack protocol). True when this
    /// CPU is running a kernel/idle task and can skip user-mode TLB flushes.
    is_lazy: AtomicBool,
    /// Process currently loaded on this CPU (or `INVALID_PROCESS_ID`).
    /// Read lock-free by `notify_mm_switch` and `current_process_on_cpu`.
    current_process_id: AtomicU32,
}

impl PerCpuTlbState {
    const fn new() -> Self {
        Self {
            queue: SpinLock::new(PendingTlbReq::new(), LOCK_LEVEL_UNORDERED),
            // Start "ack=true" so a `wait_for_acks` issued before any push
            // returns immediately (cf. asterinas `ACK_REMOTE_FLUSH = true`
            // initial value). Real waits always queue+clear first.
            ack: AtomicBool::new(true),
            is_lazy: AtomicBool::new(false),
            current_process_id: AtomicU32::new(INVALID_PROCESS_ID),
        }
    }
}

pub struct CpuMask {
    words: [AtomicU64; 4],
}

impl CpuMask {
    pub const fn new() -> Self {
        Self {
            words: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    #[inline]
    pub fn set(&self, cpu: usize) {
        if cpu >= MAX_CPUS {
            return;
        }
        let word = cpu / 64;
        let bit = cpu % 64;
        self.words[word].fetch_or(1u64 << bit, Ordering::Relaxed);
    }

    #[inline]
    pub fn clear(&self, cpu: usize) {
        if cpu >= MAX_CPUS {
            return;
        }
        let word = cpu / 64;
        let bit = cpu % 64;
        self.words[word].fetch_and(!(1u64 << bit), Ordering::Relaxed);
    }

    #[inline]
    pub fn contains(&self, cpu: usize) -> bool {
        if cpu >= MAX_CPUS {
            return false;
        }
        let word = cpu / 64;
        let bit = cpu % 64;
        (self.words[word].load(Ordering::Relaxed) & (1u64 << bit)) != 0
    }

    pub fn iter_set(&self) -> CpuMaskIter {
        CpuMaskIter {
            words: [
                self.words[0].load(Ordering::Relaxed),
                self.words[1].load(Ordering::Relaxed),
                self.words[2].load(Ordering::Relaxed),
                self.words[3].load(Ordering::Relaxed),
            ],
            next_cpu: 0,
        }
    }

    pub fn clear_all(&self) {
        for word in &self.words {
            word.store(0, Ordering::Relaxed);
        }
    }

    pub fn count(&self) -> u32 {
        let mut total = 0u32;
        for word in &self.words {
            total = total.saturating_add(word.load(Ordering::Relaxed).count_ones());
        }
        total
    }
}

impl Default for CpuMask {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CpuMaskIter {
    words: [u64; 4],
    next_cpu: usize,
}

impl Iterator for CpuMaskIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_cpu < MAX_CPUS {
            let cpu = self.next_cpu;
            self.next_cpu += 1;
            let word = cpu / 64;
            let bit = cpu % 64;
            if (self.words[word] & (1u64 << bit)) != 0 {
                return Some(cpu);
            }
        }
        None
    }
}

/// Per-process shootdown tracking.
///
/// Tracks only which CPUs currently hold a mapping for a process, so
/// broadcast flushes (`flush_all_for_process`) can target the cpumask
/// instead of every online CPU. Cross-CPU coherence for individual
/// page unmaps lives in `mm::mmu::luf`.
struct ProcessTlbInfo {
    cpumask: CpuMask,
}

impl ProcessTlbInfo {
    const fn new() -> Self {
        Self {
            cpumask: CpuMask::new(),
        }
    }
}

static PROCESS_TLB_INFO: [ProcessTlbInfo; MAX_PROCESSES] = {
    const INIT: ProcessTlbInfo = ProcessTlbInfo::new();
    [INIT; MAX_PROCESSES]
};

#[inline]
fn process_tlb_info(process_id: u32) -> Option<&'static ProcessTlbInfo> {
    let idx = process_id as usize;
    if idx >= MAX_PROCESSES {
        return None;
    }
    Some(&PROCESS_TLB_INFO[idx])
}

pub fn register_process_tlb(process_id: u32) {
    let Some(info) = process_tlb_info(process_id) else {
        return;
    };
    info.cpumask.clear_all();
}

pub fn unregister_process_tlb(process_id: u32) {
    let Some(info) = process_tlb_info(process_id) else {
        return;
    };
    info.cpumask.clear_all();
}

pub fn notify_mm_switch(old_process_id: u32, new_process_id: u32, cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }

    if old_process_id != INVALID_PROCESS_ID {
        if let Some(old_info) = process_tlb_info(old_process_id) {
            old_info.cpumask.clear(cpu_id);
        }
    }

    if new_process_id != INVALID_PROCESS_ID {
        if let Some(new_info) = process_tlb_info(new_process_id) {
            new_info.cpumask.set(cpu_id);
        }
    }

    TLB_STATE.cpu_state[cpu_id]
        .current_process_id
        .store(new_process_id, Ordering::Release);
}

pub fn current_process_on_cpu(cpu: usize) -> u32 {
    if cpu >= MAX_CPUS {
        return INVALID_PROCESS_ID;
    }
    TLB_STATE.cpu_state[cpu]
        .current_process_id
        .load(Ordering::Relaxed)
}

/// Flush request types.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlushType {
    /// No pending flush.
    None = 0,
    /// Flush a single page.
    SinglePage = 1,
    /// Flush a range of pages.
    Range = 2,
    /// Flush entire TLB (all entries).
    Full = 3,
}

impl From<u32> for FlushType {
    fn from(val: u32) -> Self {
        match val {
            1 => FlushType::SinglePage,
            2 => FlushType::Range,
            3 => FlushType::Full,
            _ => FlushType::None,
        }
    }
}

/// Global TLB shootdown state.
struct TlbShootdownState {
    /// Per-CPU flush request state.
    cpu_state: [PerCpuTlbState; MAX_CPUS],
    /// Bitmask of CPUs that are fully online and can handle TLB IPIs.
    online_cpus: CpuMask,
    /// Global sequence number for ordering.
    sequence: AtomicU64,
}

impl TlbShootdownState {
    const fn new() -> Self {
        const INIT_STATE: PerCpuTlbState = PerCpuTlbState::new();
        Self {
            cpu_state: [INIT_STATE; MAX_CPUS],
            online_cpus: CpuMask::new(),
            sequence: AtomicU64::new(0),
        }
    }
}

static TLB_STATE: TlbShootdownState = TlbShootdownState::new();

#[inline(always)]
fn flush_tlb_local_full() {
    cpu::flush_tlb_all();
}

#[inline]
fn flush_page_local(vaddr: VirtAddr) {
    cpu::invlpg(vaddr.as_u64());
}

// User-address unmap flushes go through `mm::mmu::luf::queue_unmap`
// rather than a per-process generation bump. Kept as a private alias
// above (`flush_page_local`) for the shootdown-handler path only.

/// Flush a range of pages on the local CPU.
fn flush_range_local(start: VirtAddr, end: VirtAddr) {
    let start_addr = start.as_u64();
    let end_addr = end.as_u64();

    if end_addr <= start_addr {
        return;
    }

    let page_count = ((end_addr - start_addr) + PAGE_SIZE_4KB - 1) / PAGE_SIZE_4KB;

    if page_count as usize > INVLPG_THRESHOLD {
        flush_tlb_local_full();
        return;
    }

    let mut addr = start_addr;
    while addr < end_addr {
        cpu::invlpg(addr);
        addr += PAGE_SIZE_4KB;
    }
}

// =============================================================================
// IPI-Based Shootdown (SMP)
// =============================================================================

/// Check if we're running in SMP mode (more than one CPU active).
#[inline]
pub fn is_smp_active() -> bool {
    TLB_STATE.online_cpus.count() > 1
}

#[inline]
pub fn get_active_cpu_count() -> u32 {
    TLB_STATE.online_cpus.count()
}

/// Notify the TLB subsystem that a new CPU is online.
///
/// Called during AP startup after the CPU's topology has been registered
/// via `slopos_arch::pcr::init_ap_pcr`. This only updates the TLB
/// shootdown active-CPU count; topology lives in `slopos_arch::pcr`.
pub fn notify_cpu_online_id(cpu: usize) {
    TLB_STATE.online_cpus.set(cpu);
}

pub fn notify_cpu_online() {
    notify_cpu_online_id(slopos_arch::pcr::get_current_cpu());
}

pub fn notify_cpu_offline() {
    let cpu = slopos_arch::pcr::get_current_cpu();
    TLB_STATE.online_cpus.clear(cpu);
}

fn send_shootdown_ipi_to_cpu(cpu_idx: usize) {
    let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(cpu_idx) else {
        return;
    };
    slopos_arch::pcr::send_ipi_to_cpu(apic_id, TLB_SHOOTDOWN_VECTOR);
}

/// Send the TLB-shootdown IPI to each target CPU individually instead
/// of broadcasting. Per-target sends queue independently in each
/// LAPIC's IRR so two concurrent initiators don't risk LAPIC-level
/// coalescing of their wake signals — empirically observed to be the
/// remaining failure mode after the per-target-queue race fix.
fn send_shootdown_ipi_per_target(targets: impl IntoIterator<Item = usize>) {
    for cpu_idx in targets {
        send_shootdown_ipi_to_cpu(cpu_idx);
    }
}

fn wait_for_acks(targets: impl IntoIterator<Item = usize>, initiator_cpu: usize) -> TlbResult {
    // SYSCALL entry clears IF (SFMASK bit 9) so callers often arrive
    // with interrupts disabled.  Re-enable them for the spin-wait so
    // that (a) we can receive TLB IPIs from other CPUs that need our
    // ack, and (b) mouse/keyboard/timer IRQs keep firing.  The syscall
    // path holds NO_PREEMPT, so no context switch will occur.
    let was_enabled = cpu::are_interrupts_enabled();
    if !was_enabled {
        cpu::enable_interrupts();
    }

    let mut result = Ok(());
    'targets: for cpu_idx in targets {
        if cpu_idx >= MAX_CPUS || cpu_idx == initiator_cpu {
            continue;
        }

        // Re-send the shootdown IPI if the ack does not arrive within a
        // bounded spin. A single IPI delivery is NOT trustworthy: under
        // interaction load the initiator's LAPIC ICR can stay busy past
        // `wait_icr_idle`'s poll cap (the send then proceeds but the edge
        // may be dropped), and a coalescing/timing edge can leave a target
        // that never runs the handler — the request stays queued, `ack`
        // stays false, and the initiator spins forever (observed live:
        // CPU0 wedged in this loop on the munmap path while every peer sat
        // idle-HLT). Re-sending is idempotent and safe: the request is
        // still queued, so the handler `take`s it and stamps `ack=true`;
        // if it was already handled, `take` returns `None` and the store
        // is a harmless no-op. This converts a lost IPI from an unbounded
        // hang into a few-millisecond recovery.
        const RESEND_SPIN: u64 = 1_000_000;
        const MAX_RESENDS: u64 = 256; // bounded recovery (~sub-second) before declaring the CPU dead
        let mut spin_count: u64 = 0;
        let mut resends: u64 = 0;
        while !TLB_STATE.cpu_state[cpu_idx].ack.load(Ordering::Acquire) {
            // Reliable Abort Core interlock: once any CPU is driving a fatal
            // panic, abandon the wait. A panicking initiator must not spin on
            // an ack from a CPU it is about to NMI-stop, and a non-panicking
            // initiator is unblocked by the stop handler's force-ack. Without
            // this, the panicking CPU wedges here behind a peer in its own
            // panic — the exact cross-CPU lockup the abort core dissolves.
            if slopos_ostd::panic::panic_owner_claimed() {
                break 'targets;
            }
            spin_count = spin_count.wrapping_add(1);
            if spin_count >= RESEND_SPIN {
                spin_count = 0;
                resends += 1;
                if resends > MAX_RESENDS {
                    // The target genuinely never acked across thousands of
                    // re-sends — surface it as a fault rather than hang the
                    // initiator (which still holds the VM lock) forever.
                    klog_warn!(
                        "TLB: CPU {} never acked shootdown after {} re-sends; giving up",
                        cpu_idx,
                        resends
                    );
                    result = Err(TlbShootdownTimeout { cpu_idx, resends });
                    break 'targets;
                }
                // Re-arm: the request is still in the slot if unhandled.
                send_shootdown_ipi_to_cpu(cpu_idx);
            }
            cpu::pause();
        }

        // DELIBERATELY no `ack.store(false)` here. The lock-protected
        // push (in `queue_request_for_cpu`) is the sole writer that
        // transitions ack from true→false. If we reset here, a second
        // initiator that has already read `true` could see our `false`
        // and spin forever waiting for an IPI handler that already ran.
        // See the file-level race description.
    }

    if !was_enabled {
        cpu::disable_interrupts();
    }
    result
}

/// Force this CPU's shootdown ack to `true` so any outstanding initiator stops
/// waiting on us. Called by the panic-stop NMI handler just before halting,
/// since a halted CPU can no longer run the IPI handler that would normally
/// ack. Set-only — it never clears an ack to `false`, preserving the
/// "ack true→false only under the queue lock" invariant that keeps a second
/// merged-IPI initiator from spinning forever.
pub fn force_ack_local_shootdowns(cpu_idx: usize) {
    if cpu_idx < MAX_CPUS {
        TLB_STATE.cpu_state[cpu_idx]
            .ack
            .store(true, Ordering::Release);
    }
}

fn promote_remote_flush_type(flush_type: FlushType, start: u64, end: u64) -> FlushType {
    if flush_type != FlushType::Range || end <= start {
        return flush_type;
    }

    let page_count = ((end - start) + PAGE_SIZE_4KB - 1) / PAGE_SIZE_4KB;
    if page_count as usize > INVLPG_THRESHOLD {
        FlushType::Full
    } else {
        FlushType::Range
    }
}

fn should_flush_tlb_for_process(cpu: usize, process_id: u32) -> bool {
    if cpu >= MAX_CPUS || process_id == INVALID_PROCESS_ID {
        return false;
    }
    if !should_flush_tlb(cpu) {
        return false;
    }
    let Some(info) = process_tlb_info(process_id) else {
        return false;
    };
    info.cpumask.contains(cpu)
}

pub fn should_flush_tlb(cpu: usize) -> bool {
    if cpu >= MAX_CPUS {
        return false;
    }
    !TLB_STATE.cpu_state[cpu].is_lazy.load(Ordering::Acquire)
}

pub fn enter_lazy_tlb(cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    TLB_STATE.cpu_state[cpu]
        .is_lazy
        .store(true, Ordering::Release);
}

pub fn exit_lazy_tlb(cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }

    let state = &TLB_STATE.cpu_state[cpu];
    state.is_lazy.store(false, Ordering::Release);

    // Cross-CPU coherence for user-space unmaps is driven entirely
    // by `mm::mmu::luf::drain_by_phys_cross_cpu` at frame reuse. No
    // generation-based catch-up needed on lazy-TLB exit — if this
    // CPU held a stale translation pointing at a now-freed frame,
    // the drain IPI already invalidated it before the frame was
    // handed out to a new owner.
}

fn queue_request_for_cpu(
    cpu_idx: usize,
    flush_type: FlushType,
    start: u64,
    end: u64,
    asid: u64,
    _process_id: u32,
) {
    if cpu_idx >= MAX_CPUS {
        return;
    }
    let op = match flush_type {
        FlushType::None => return,
        FlushType::Full => PendingFlush::Full,
        FlushType::SinglePage => PendingFlush::SinglePage { start },
        FlushType::Range => {
            if end <= start {
                return;
            }
            PendingFlush::Range { start, end }
        }
    };
    let slot = &TLB_STATE.cpu_state[cpu_idx];

    // Hold the lock for `push` + `ack=false`. The handler holds the
    // same lock for `take` + `ack=true`. That serialisation is what
    // closes the multi-initiator race (see PerCpuTlbState comment).
    let mut queue = slot.queue.lock();
    queue.push(op, asid);
    slot.ack.store(false, Ordering::Relaxed);
    drop(queue);
}

fn broadcast_flush_request(flush_type: FlushType, start: u64, end: u64, asid: u64) {
    let flush_type = promote_remote_flush_type(flush_type, start, end);

    for cpu_idx in TLB_STATE.online_cpus.iter_set() {
        queue_request_for_cpu(cpu_idx, flush_type, start, end, asid, INVALID_PROCESS_ID);
    }

    core::sync::atomic::fence(Ordering::SeqCst);
}

fn targeted_flush_request(
    process_id: u32,
    flush_type: FlushType,
    start: u64,
    end: u64,
) -> TlbResult {
    let Some(info) = process_tlb_info(process_id) else {
        return Ok(());
    };

    let flush_type = promote_remote_flush_type(flush_type, start, end);
    let initiator = slopos_arch::pcr::get_current_cpu();

    if should_flush_tlb_for_process(initiator, process_id) {
        match flush_type {
            FlushType::SinglePage => flush_page_local(VirtAddr::new(start)),
            FlushType::Range => flush_range_local(VirtAddr::new(start), VirtAddr::new(end)),
            FlushType::Full => flush_tlb_local_full(),
            FlushType::None => {}
        }
    }

    // Allocate the per-CPU target list on the heap: a stack-resident
    // `[usize; MAX_CPUS]` is 2 KiB on its own and pushes this function
    // over the stack-sizes gate.
    let mut targets = match slopos_ostd::KVec::<usize>::zeroed(MAX_CPUS) {
        Ok(v) => v,
        Err(_) => {
            klog_warn!("tlb: targeted_flush_request alloc failed; falling back to local");
            return Ok(());
        }
    };
    let mut target_count = 0usize;

    for cpu_idx in info.cpumask.iter_set() {
        if cpu_idx == initiator {
            continue;
        }
        if !slopos_arch::pcr::is_cpu_online(cpu_idx) || !should_flush_tlb(cpu_idx) {
            continue;
        }

        queue_request_for_cpu(cpu_idx, flush_type, start, end, 0, process_id);
        if target_count < MAX_CPUS {
            targets[target_count] = cpu_idx;
            target_count += 1;
        }
    }

    if target_count == 0 {
        return Ok(());
    }

    core::sync::atomic::fence(Ordering::SeqCst);
    for cpu_idx in targets.iter().take(target_count) {
        send_shootdown_ipi_to_cpu(*cpu_idx);
    }
    wait_for_acks(targets[..target_count].iter().copied(), initiator)
}

// =============================================================================
// Public TLB Flush API
// =============================================================================

/// Initialize the TLB subsystem.
/// Called during kernel boot.
pub fn init() {
    detect_features();
    TLB_STATE.online_cpus.set(0);
    klog_info!("TLB: Subsystem initialized");
}

/// Iterate online CPUs excluding `exclude` (typically the initiator).
///
/// Returning an iterator — rather than a stack-resident `[usize; MAX_CPUS]`
/// array — keeps TLB flush entry points off the 2 KiB-per-call frame
/// hit that a full mask copy would incur.
fn online_cpus(exclude: usize) -> impl Iterator<Item = usize> + 'static {
    TLB_STATE
        .online_cpus
        .iter_set()
        .filter(move |&cpu| cpu != exclude)
}

/// Flush a single page from all CPUs' TLBs.
///
/// This is the primary function called after unmapping a page.
/// On uniprocessor systems, it performs a local invlpg.
/// On SMP systems, it broadcasts an IPI to all CPUs.
#[inline]
fn handle_tlb_result(result: TlbResult, context: &str) {
    if let Err(err) = result {
        panic!(
            "{}: CPU {} did not ack TLB shootdown after {} re-sends",
            context, err.cpu_idx, err.resends
        );
    }
}

pub fn try_flush_page(vaddr: VirtAddr) -> TlbResult {
    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::SinglePage, vaddr.as_u64(), 0, 0);
        // Per-target IPIs instead of broadcast: at the LAPIC level a
        // broadcast IPI to vector V can deliver-then-coalesce with a
        // second broadcast to the same vector before the first handler
        // has cleared IRR, dropping the second wake. Per-target sends
        // queue independently in each LAPIC's IRR and don't collapse
        // across initiators.
        send_shootdown_ipi_per_target(online_cpus(initiator));
        // Overlap our own invlpg with the remote CPUs' handlers instead
        // of running it serially up front (Amit et al. EuroSys '20).
        flush_page_local(vaddr);
        wait_for_acks(online_cpus(initiator), initiator)
    } else {
        flush_page_local(vaddr);
        Ok(())
    }
}

pub fn flush_page(vaddr: VirtAddr) {
    handle_tlb_result(try_flush_page(vaddr), "flush_page");
}

pub fn try_flush_page_for_process(process_id: u32, vaddr: VirtAddr) -> TlbResult {
    targeted_flush_request(process_id, FlushType::SinglePage, vaddr.as_u64(), 0)
}

pub fn flush_page_for_process(process_id: u32, vaddr: VirtAddr) {
    handle_tlb_result(
        try_flush_page_for_process(process_id, vaddr),
        "flush_page_for_process",
    );
}

/// Flush a range of pages from all CPUs' TLBs.
///
/// For small ranges, invalidates each page individually.
/// For large ranges, performs a full TLB flush.
pub fn try_flush_range(start: VirtAddr, end: VirtAddr) -> TlbResult {
    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Range, start.as_u64(), end.as_u64(), 0);
        send_shootdown_ipi_per_target(online_cpus(initiator));
        flush_range_local(start, end);
        wait_for_acks(online_cpus(initiator), initiator)
    } else {
        flush_range_local(start, end);
        Ok(())
    }
}

pub fn flush_range(start: VirtAddr, end: VirtAddr) {
    handle_tlb_result(try_flush_range(start, end), "flush_range");
}

pub fn try_flush_range_for_process(process_id: u32, start: VirtAddr, end: VirtAddr) -> TlbResult {
    targeted_flush_request(process_id, FlushType::Range, start.as_u64(), end.as_u64())
}

pub fn flush_range_for_process(process_id: u32, start: VirtAddr, end: VirtAddr) {
    handle_tlb_result(
        try_flush_range_for_process(process_id, start, end),
        "flush_range_for_process",
    );
}

/// Flush the entire TLB on all CPUs.
///
/// This is the most expensive operation but sometimes necessary,
/// e.g., when changing CR3 or modifying many pages.
pub fn try_flush_all() -> TlbResult {
    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, 0);
        send_shootdown_ipi_per_target(online_cpus(initiator));
        flush_tlb_local_full();
        wait_for_acks(online_cpus(initiator), initiator)
    } else {
        flush_tlb_local_full();
        Ok(())
    }
}

pub fn flush_all() {
    handle_tlb_result(try_flush_all(), "flush_all");
}

pub fn try_flush_all_for_process(process_id: u32) -> TlbResult {
    targeted_flush_request(process_id, FlushType::Full, 0, 0)
}

pub fn flush_all_for_process(process_id: u32) {
    handle_tlb_result(
        try_flush_all_for_process(process_id),
        "flush_all_for_process",
    );
}

/// Flush TLB entries for a specific address space (ASID/CR3) on all CPUs.
///
/// This is useful when destroying a process - we only need to flush
/// entries associated with that process's page tables.
pub fn try_flush_asid(asid: u64) -> TlbResult {
    let current_cr3 = cpu::read_cr3();
    let local_needs_flush = (current_cr3 & !0xFFF) == (asid & !0xFFF);

    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, asid);
        send_shootdown_ipi_per_target(online_cpus(initiator));
        if local_needs_flush {
            flush_tlb_local_full();
        }
        wait_for_acks(online_cpus(initiator), initiator)
    } else if local_needs_flush {
        flush_tlb_local_full();
        Ok(())
    } else {
        Ok(())
    }
}

pub fn flush_asid(asid: u64) {
    handle_tlb_result(try_flush_asid(asid), "flush_asid");
}

/// Handle TLB shootdown IPI on the receiving CPU.
///
/// This is called from the interrupt handler when a TLB shootdown
/// IPI is received. It processes the pending flush request and
/// sends acknowledgment.
///
/// # Safety
///
/// Must be called from interrupt context with interrupts disabled.
pub fn handle_shootdown_ipi(cpu_idx: usize) {
    if cpu_idx >= MAX_CPUS {
        return;
    }

    let slot = &TLB_STATE.cpu_state[cpu_idx];

    // Take the merged pending request and stamp ACK=true while holding the
    // lock. This pairs with `queue_request_for_cpu`'s `push + clear-ACK`
    // under the same lock to give per-initiator visibility even when LAPIC
    // coalesces multiple IPIs into one handler invocation.
    //
    // Early ACK (Amit et al., EuroSys '20): we stamp the ack BEFORE doing
    // the actual flush. Safe because we are still in IRQ context — IRETQ
    // is a serialising instruction, so any code that subsequently runs on
    // this CPU (kernel or user) observes the completed local flush. The
    // initiator's "ack=true" therefore means "this CPU will flush before
    // resuming any non-handler work", which is the synchronisation guarantee
    // page-table mutators actually need.
    let (op, asid) = {
        let mut queue = slot.queue.lock();
        let taken = queue.take();
        slot.ack.store(true, Ordering::Release);
        taken
    };

    // ASID filter: a non-zero `asid` means the initiator only cares if
    // this CPU's current address space matches. We still acked (above)
    // because the initiator's wait is bounded on the ACK flag alone, not
    // on whether the flush was actually performed.
    if asid != 0 {
        let local_cr3 = cpu::read_cr3();
        if (local_cr3 & !0xFFF) != (asid & !0xFFF) {
            return;
        }
    }

    match op {
        PendingFlush::None => {}
        PendingFlush::SinglePage { start } => flush_page_local(VirtAddr::new(start)),
        PendingFlush::Range { start, end } => {
            flush_range_local(VirtAddr::new(start), VirtAddr::new(end))
        }
        PendingFlush::Full => flush_tlb_local_full(),
    }
}

/// Batched TLB flush for multiple pages.
///
/// Collects multiple flush requests and executes them efficiently.
/// If the batch exceeds the threshold, performs a full flush instead.
pub struct TlbFlushBatch {
    pages: [VirtAddr; INVLPG_THRESHOLD],
    count: usize,
}

impl TlbFlushBatch {
    /// Create a new empty batch.
    pub const fn new() -> Self {
        Self {
            pages: [VirtAddr::NULL; INVLPG_THRESHOLD],
            count: 0,
        }
    }

    /// Add a page to the batch.
    /// If the batch is full, it will be flushed as a full TLB invalidation.
    pub fn add(&mut self, vaddr: VirtAddr) {
        if self.count < INVLPG_THRESHOLD {
            self.pages[self.count] = vaddr;
            self.count += 1;
        }
    }

    /// Flush all batched pages.
    pub fn finish(&mut self) {
        if self.count == 0 {
            return;
        }

        if self.count >= INVLPG_THRESHOLD {
            flush_all();
        } else if self.count == 1 {
            flush_page(self.pages[0]);
        } else {
            let mut min_addr = self.pages[0].as_u64();
            let mut max_addr = min_addr + PAGE_SIZE_4KB;

            for i in 1..self.count {
                let addr = self.pages[i].as_u64();
                if addr < min_addr {
                    min_addr = addr;
                }
                if addr + PAGE_SIZE_4KB > max_addr {
                    max_addr = addr + PAGE_SIZE_4KB;
                }
            }

            flush_range(VirtAddr::new(min_addr), VirtAddr::new(max_addr));
        }

        self.count = 0;
    }
}

impl Default for TlbFlushBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TlbFlushBatch {
    fn drop(&mut self) {
        if self.count > 0 {
            self.finish();
        }
    }
}
