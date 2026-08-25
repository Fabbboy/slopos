//! Cross-CPU TLB invalidation: a page-table change is not complete until
//! every CPU that may have cached the translation has invalidated it.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::addr::VirtAddr;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_arch::cpu;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};
use slopos_ostd::{klog_debug, klog_info, klog_warn};

use crate::memory_layout_defs::MAX_PROCESSES;
use crate::paging_defs::PAGE_SIZE_4KB;

/// Sends a TLB-shootdown IPI; the argument is the IPI vector number.
pub type SendIpiFn = fn(u8);

static IPI_SENDER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Must be called by the APIC driver during initialization.
pub fn register_ipi_sender(sender: SendIpiFn) {
    IPI_SENDER.store(sender as *mut (), Ordering::Release);
    klog_debug!("TLB: IPI sender registered");
}

/// Above this many pages, a full flush (CR3 reload) beats per-page invlpg.
const INVLPG_THRESHOLD: usize = 32;

pub use slopos_arch::arch::idt::TLB_SHOOTDOWN_VECTOR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbShootdownTimeout {
    pub cpu_idx: usize,
    pub resends: u64,
}

type TlbResult = Result<(), TlbShootdownTimeout>;

use slopos_arch::cpu::cpuid::{
    CPUID_FEAT_ECX_PCID, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_INVPCID,
};

struct TlbFeatures {
    invpcid_supported: AtomicBool,
    pcid_supported: AtomicBool,
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

#[inline]
pub fn has_invpcid() -> bool {
    if !TLB_FEATURES.initialized.load(Ordering::Acquire) {
        detect_features();
    }
    TLB_FEATURES.invpcid_supported.load(Ordering::Relaxed)
}

#[inline]
pub fn has_pcid() -> bool {
    if !TLB_FEATURES.initialized.load(Ordering::Acquire) {
        detect_features();
    }
    TLB_FEATURES.pcid_supported.load(Ordering::Relaxed)
}

/// Merged pending flush for one target CPU; a second push promotes to `Full`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingFlush {
    None,
    /// Separate from `Range` because the highest user page would give an `end`
    /// of 0x8000000000000000, which is non-canonical and panics `VirtAddr::new`.
    SinglePage {
        start: u64,
    },
    /// Flush `[start, end)`. Caller guarantees `start < end` and canonical `end`.
    Range {
        start: u64,
        end: u64,
    },
    Full,
}

/// `asid == 0` means "any address space"; a non-zero `asid` is flushed only by
/// a CPU whose live CR3 matches `asid & !0xFFF`.
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

    fn take(&mut self) -> (PendingFlush, u64) {
        let op = core::mem::replace(&mut self.op, PendingFlush::None);
        let asid = core::mem::replace(&mut self.asid, 0);
        (op, asid)
    }
}

/// Per-CPU TLB shootdown slot.
///
/// `ack` is set `true` only by the handler and cleared only by an initiator's
/// push, both under `queue`'s lock; `wait_for_acks` is a pure reader. That
/// serialisation is what keeps a LAPIC-coalesced IPI from letting one
/// initiator clear an ack another has already consumed.
#[repr(C, align(64))]
struct PerCpuTlbState {
    queue: SpinLock<PendingTlbReq>,
    ack: AtomicBool,
    /// True while this CPU runs a kernel/idle task and can skip user flushes.
    is_lazy: AtomicBool,
    /// Process loaded on this CPU, or `INVALID_PROCESS_ID`.
    current_process_id: AtomicU32,
    /// Shootdown key of the address space loaded on this CPU, as `slot + 1`;
    /// 0 means none.
    current_process_key: AtomicU32,
}

impl PerCpuTlbState {
    const fn new() -> Self {
        Self {
            queue: SpinLock::new(
                PendingTlbReq::new(),
                lock_class!("PerCpuTlbState.queue", LOCK_LEVEL_UNORDERED),
            ),
            // Initial `true`: a wait issued before any push must not block.
            ack: AtomicBool::new(true),
            is_lazy: AtomicBool::new(false),
            current_process_id: AtomicU32::new(INVALID_PROCESS_ID),
            current_process_key: AtomicU32::new(0),
        }
    }

    #[inline]
    fn loaded_process_key(&self) -> Option<TlbProcessKey> {
        match self.current_process_key.load(Ordering::Relaxed) {
            0 => None,
            encoded => TlbProcessKey::from_slot(encoded - 1),
        }
    }

    #[inline]
    fn store_process_key(&self, key: Option<TlbProcessKey>) {
        let encoded = key.map_or(0, |k| k.slot() + 1);
        self.current_process_key.store(encoded, Ordering::Relaxed);
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

/// CPUs holding a mapping for a process, so a broadcast flush can target the
/// mask rather than every online CPU. Per-page unmap coherence lives in
/// `mm::mmu::luf`.
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

/// A bounds-proved index into the per-process shootdown table.
///
/// Minting is the only fallible step, so a key that exists indexes the table
/// totally and no flush path has a "no entry" arm to mistake for "nothing to do".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlbProcessKey(u32);

impl TlbProcessKey {
    pub fn from_slot(slot: u32) -> Option<Self> {
        if (slot as usize) < MAX_PROCESSES {
            Some(Self(slot))
        } else {
            None
        }
    }

    #[inline]
    pub fn slot(self) -> u32 {
        self.0
    }
}

#[inline]
fn process_tlb_info(key: TlbProcessKey) -> &'static ProcessTlbInfo {
    &PROCESS_TLB_INFO[key.0 as usize]
}

pub fn register_process_tlb(key: TlbProcessKey) {
    process_tlb_info(key).cpumask.clear_all();
}

pub fn unregister_process_tlb(key: TlbProcessKey) {
    process_tlb_info(key).cpumask.clear_all();
}

/// Test hook: the shootdown mask is otherwise write-only outside this module.
#[cfg(feature = "test-hooks")]
pub fn process_tlb_cpumask_count(key: TlbProcessKey) -> u32 {
    process_tlb_info(key).cpumask.count()
}

/// Record that `cpu_id` is switching into `new_key`'s address space, or out of
/// any address space when `new_key` is `None`.
pub fn notify_mm_switch(new_key: Option<TlbProcessKey>, new_process_id: u32, cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let state = &TLB_STATE.cpu_state[cpu_id];

    if let Some(old_key) = state.loaded_process_key() {
        process_tlb_info(old_key).cpumask.clear(cpu_id);
    }
    if let Some(key) = new_key {
        process_tlb_info(key).cpumask.set(cpu_id);
    }

    state.store_process_key(new_key);
    state
        .current_process_id
        .store(new_process_id, Ordering::Release);
}

/// Test hook: drop `cpu_id`'s record of the address space it last ran, leaving
/// every process's shootdown mask untouched.
///
/// `TlbProcessKey` is a bare slot index with no generation, so once the process
/// a CPU was parked in is gone, `notify_mm_switch(None, ..)` would clear that
/// slot's mask bit on behalf of whichever process has since taken the slot.
#[cfg(feature = "test-hooks")]
pub fn forget_cpu_process_key(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let state = &TLB_STATE.cpu_state[cpu_id];
    state.store_process_key(None);
    state
        .current_process_id
        .store(INVALID_PROCESS_ID, Ordering::Release);
}

pub fn current_process_on_cpu(cpu: usize) -> u32 {
    if cpu >= MAX_CPUS {
        return INVALID_PROCESS_ID;
    }
    TLB_STATE.cpu_state[cpu]
        .current_process_id
        .load(Ordering::Relaxed)
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlushType {
    None = 0,
    SinglePage = 1,
    Range = 2,
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

struct TlbShootdownState {
    cpu_state: [PerCpuTlbState; MAX_CPUS],
    /// CPUs online far enough to service TLB IPIs.
    online_cpus: CpuMask,
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

/// Every non-global translation in every cached address space, not just the one
/// CR3 names — see [`crate::mmu::asid::flush_local_all_contexts`].
#[inline(always)]
fn flush_tlb_local_full() {
    crate::mmu::asid::flush_local_all_contexts();
}

/// Local-only full flush (no IPI): an AP joining the shootdown-target set
/// drops translations cached while it was not yet a target.
#[inline]
pub fn flush_local_all() {
    flush_tlb_local_full();
}

#[inline]
fn flush_page_local(vaddr: VirtAddr) {
    cpu::invlpg(vaddr.as_u64());
}

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

#[inline]
pub fn is_smp_active() -> bool {
    TLB_STATE.online_cpus.count() > 1
}

#[inline]
pub fn get_active_cpu_count() -> u32 {
    TLB_STATE.online_cpus.count()
}

/// Must run after the CPU's topology is registered (`pcr::init_ap_pcr`).
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

/// Per-target sends, never a broadcast: each LAPIC queues its own IRR entry, so
/// two concurrent initiators' wakes cannot coalesce at the LAPIC.
fn send_shootdown_ipi_per_target(targets: impl IntoIterator<Item = usize>) {
    for cpu_idx in targets {
        send_shootdown_ipi_to_cpu(cpu_idx);
    }
}

/// Reads state the target published on its way in, so it works even when the
/// target takes no interrupts — the case that matters.
fn describe_deaf_cpu(cpu_idx: usize) {
    let mut any_held = false;
    slopos_ostd::sync::for_each_held_lock_name_for_cpu(cpu_idx, |name| {
        any_held = true;
        klog_warn!("TLB: CPU {} holds lock '{}'", cpu_idx, name);
    });
    if !any_held {
        klog_warn!("TLB: CPU {} holds no tracked lock", cpu_idx);
    }

    let mut hops = [slopos_ostd::watchdog::WaitHop {
        cpu: 0,
        seq: 0,
        lock: 0,
    }; slopos_ostd::watchdog::MAX_WAIT_HOPS];
    let (len, end) = slopos_ostd::watchdog::wait_chain_snapshot(cpu_idx, &mut hops);
    for hop in hops.iter().take(len) {
        klog_warn!("TLB: CPU {} waits on lock {:#x}", hop.cpu, hop.lock);
    }
    klog_warn!("TLB: CPU {} wait chain ends {:?}", cpu_idx, end);
}

fn report_probe_rip(cpu_idx: usize) {
    let Some(rip) = slopos_ostd::watchdog::probe_rip(cpu_idx) else {
        klog_warn!("TLB: CPU {} never took a probe NMI", cpu_idx);
        return;
    };
    match slopos_ostd::ksym::lookup(rip) {
        Some(sym) => klog_warn!(
            "TLB: CPU {} stopped at {:#x} <{}+{:#x}>",
            cpu_idx,
            rip,
            sym.symbol,
            sym.offset
        ),
        None => klog_warn!("TLB: CPU {} stopped at {:#x}", cpu_idx, rip),
    }
}

/// An NMI is unmaskable, so a CPU that misses the budget is already inside one.
fn wait_for_probe_answer(cpu_idx: usize) {
    const PROBE_ANSWER_SPIN: u64 = 20_000_000;
    for _ in 0..PROBE_ANSWER_SPIN {
        if slopos_ostd::watchdog::probe_disposition(cpu_idx)
            == slopos_ostd::watchdog::NmiDisposition::Unsolicited
        {
            return;
        }
        service_local_shootdown_queue();
        cpu::pause();
    }
}

fn wait_for_acks(targets: impl IntoIterator<Item = usize>, initiator_cpu: usize) -> TlbResult {
    // Runs with whatever IF the caller established and never force-enables:
    // callers legitimately hold IRQ-disabling SpinLocks across a flush.
    let mut result = Ok(());
    'targets: for cpu_idx in targets {
        if cpu_idx >= MAX_CPUS || cpu_idx == initiator_cpu {
            continue;
        }

        // One IPI delivery is not trustworthy: a busy ICR or a coalescing edge
        // can leave the request queued with no handler run. Re-sending is
        // idempotent — an already-handled slot `take`s to nothing.
        const RESEND_SPIN: u64 = 1_000_000;
        const MAX_RESENDS: u64 = 256; // bounded recovery (~seconds) before declaring the CPU dead
        let mut spin_count: u64 = 0;
        let mut resends: u64 = 0;
        while !TLB_STATE.cpu_state[cpu_idx].ack.load(Ordering::Acquire) {
            // Abandon the wait once any CPU owns a fatal panic: a panicking
            // initiator must not spin on a CPU it is about to NMI-stop.
            if slopos_ostd::panic::panic_owner_claimed() {
                break 'targets;
            }
            service_local_shootdown_queue();
            spin_count = spin_count.wrapping_add(1);
            if spin_count >= RESEND_SPIN {
                spin_count = 0;
                resends += 1;
                if resends > MAX_RESENDS {
                    // NMI the target so its watchdog handler dumps the context.
                    klog_warn!(
                        "TLB: CPU {} never acked shootdown after {} re-sends; giving up",
                        cpu_idx,
                        resends
                    );
                    slopos_ostd::sync::for_each_held_lock_name(|name| {
                        klog_warn!(
                            "TLB: initiator CPU {} holds lock '{}' during the wait",
                            initiator_cpu,
                            name
                        );
                    });
                    describe_deaf_cpu(cpu_idx);
                    // Arm before sending, or the target classifies the NMI as
                    // unsolicited; a refused arm means a probe is already in
                    // flight.
                    if slopos_ostd::watchdog::arm_probe(
                        cpu_idx,
                        slopos_ostd::watchdog::NmiDisposition::TlbLadder,
                    ) {
                        match slopos_arch::pcr::apic_id_from_cpu_index(cpu_idx) {
                            Some(apic_id) => {
                                slopos_arch::pcr::send_nmi_to_cpu(apic_id);
                                wait_for_probe_answer(cpu_idx);
                            }
                            None => slopos_ostd::watchdog::release_probe(cpu_idx),
                        }
                    } else {
                        klog_warn!(
                            "TLB: CPU {} already had an NMI probe in flight; no fresh context",
                            cpu_idx
                        );
                    }
                    report_probe_rip(cpu_idx);
                    result = Err(TlbShootdownTimeout { cpu_idx, resends });
                    break 'targets;
                }
                send_shootdown_ipi_to_cpu(cpu_idx);
            }
            cpu::pause();
        }

        // Deliberately no `ack.store(false)` here: only the lock-protected push
        // transitions ack true→false (see `PerCpuTlbState`).
    }

    result
}

/// Service this CPU's pending shootdown, if any. Registered as the OSTD
/// contended-spin relax hook in [`init`] and polled by [`wait_for_acks`], so a
/// CPU busy-waiting on any SpinLock still acks — which is what makes a remote
/// flush safe to issue while holding an IRQ-disabling lock.
pub fn service_local_shootdown_queue() {
    let cpu_idx = slopos_arch::pcr::get_current_cpu();
    if cpu_idx >= MAX_CPUS {
        return;
    }
    let slot = &TLB_STATE.cpu_state[cpu_idx];
    if slot.ack.load(Ordering::Acquire) {
        return;
    }
    // try_lock, not lock: this runs from inside contended-spin loops, so a
    // blocking acquire would take a second ticket beneath the outer one and
    // deadlock this CPU against itself.
    let Some(mut queue) = slot.queue.try_lock() else {
        return;
    };
    let (op, asid) = queue.take();
    slot.ack.store(true, Ordering::Release);
    drop(queue);

    // Early ack holds here too: the flush below completes before this CPU
    // resumes the work it was spinning on.
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

/// Force this CPU's ack so outstanding initiators stop waiting on it; the
/// panic-stop NMI handler calls it before halting. Set-only: never clears an
/// ack, preserving the "true→false only under the queue lock" invariant.
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

fn should_flush_tlb_for_process(cpu: usize, key: TlbProcessKey) -> bool {
    if cpu >= MAX_CPUS {
        return false;
    }
    if !should_flush_tlb(cpu) {
        return false;
    }
    process_tlb_info(key).cpumask.contains(cpu)
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

    // No catch-up flush needed: frame reuse is gated on `mm::mmu::quiesce`, and
    // this CPU cannot have acked that epoch without having invalidated first.
}

fn queue_request_for_cpu(cpu_idx: usize, flush_type: FlushType, start: u64, end: u64, asid: u64) {
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

    let mut queue = slot.queue.lock();
    queue.push(op, asid);
    slot.ack.store(false, Ordering::Relaxed);
    drop(queue);
}

fn broadcast_flush_request(flush_type: FlushType, start: u64, end: u64, asid: u64) {
    let flush_type = promote_remote_flush_type(flush_type, start, end);

    for cpu_idx in TLB_STATE.online_cpus.iter_set() {
        queue_request_for_cpu(cpu_idx, flush_type, start, end, asid);
    }

    core::sync::atomic::fence(Ordering::SeqCst);
}

fn targeted_flush_request(
    key: TlbProcessKey,
    flush_type: FlushType,
    start: u64,
    end: u64,
) -> TlbResult {
    let info = process_tlb_info(key);

    let flush_type = promote_remote_flush_type(flush_type, start, end);
    let initiator = slopos_arch::pcr::get_current_cpu();

    if should_flush_tlb_for_process(initiator, key) {
        match flush_type {
            FlushType::SinglePage => flush_page_local(VirtAddr::new(start)),
            FlushType::Range => flush_range_local(VirtAddr::new(start), VirtAddr::new(end)),
            FlushType::Full => flush_tlb_local_full(),
            FlushType::None => {}
        }
    }

    // A bitmap, not an array or a heap vector: `[usize; MAX_CPUS]` is 2 KiB and
    // blows the stack-sizes gate, and callers hold an IRQ-disabling lock, so
    // allocating here has no honest failure behaviour.
    let targets = CpuMask::new();
    let mut target_count = 0usize;

    for cpu_idx in info.cpumask.iter_set() {
        if cpu_idx == initiator {
            continue;
        }
        if !slopos_arch::pcr::is_cpu_online(cpu_idx) || !should_flush_tlb(cpu_idx) {
            continue;
        }

        queue_request_for_cpu(cpu_idx, flush_type, start, end, 0);
        targets.set(cpu_idx);
        target_count += 1;
    }

    if target_count == 0 {
        return Ok(());
    }

    core::sync::atomic::fence(Ordering::SeqCst);
    for cpu_idx in targets.iter_set() {
        send_shootdown_ipi_to_cpu(cpu_idx);
    }
    wait_for_acks(targets.iter_set(), initiator)
}

pub fn init() {
    detect_features();
    TLB_STATE.online_cpus.set(0);
    slopos_ostd::sync::register_spin_relax_hook(service_local_shootdown_queue);
    klog_info!("TLB: Subsystem initialized");
}

/// Online CPUs except `exclude`. An iterator rather than a `[usize; MAX_CPUS]`
/// array, which would put callers over the 2 KiB stack-frame gate.
fn online_cpus(exclude: usize) -> impl Iterator<Item = usize> + 'static {
    TLB_STATE
        .online_cpus
        .iter_set()
        .filter(move |&cpu| cpu != exclude)
}

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
        send_shootdown_ipi_per_target(online_cpus(initiator));
        // Overlap the local invlpg with the remote handlers rather than running
        // it serially first (Amit et al., EuroSys '20).
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

pub fn try_flush_page_for_process(key: TlbProcessKey, vaddr: VirtAddr) -> TlbResult {
    targeted_flush_request(key, FlushType::SinglePage, vaddr.as_u64(), 0)
}

pub fn flush_page_for_process(key: TlbProcessKey, vaddr: VirtAddr) {
    handle_tlb_result(
        try_flush_page_for_process(key, vaddr),
        "flush_page_for_process",
    );
}

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

pub fn try_flush_range_for_process(
    key: TlbProcessKey,
    start: VirtAddr,
    end: VirtAddr,
) -> TlbResult {
    targeted_flush_request(key, FlushType::Range, start.as_u64(), end.as_u64())
}

pub fn flush_range_for_process(key: TlbProcessKey, start: VirtAddr, end: VirtAddr) {
    handle_tlb_result(
        try_flush_range_for_process(key, start, end),
        "flush_range_for_process",
    );
}

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

pub fn try_flush_all_for_process(key: TlbProcessKey) -> TlbResult {
    targeted_flush_request(key, FlushType::Full, 0, 0)
}

pub fn flush_all_for_process(key: TlbProcessKey) {
    handle_tlb_result(try_flush_all_for_process(key), "flush_all_for_process");
}

/// `asid` is a CR3 value; a CPU flushes only if its live CR3 matches it.
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

/// # Safety
///
/// Must be called from interrupt context with interrupts disabled.
pub fn handle_shootdown_ipi(cpu_idx: usize) {
    if cpu_idx >= MAX_CPUS {
        return;
    }

    let slot = &TLB_STATE.cpu_state[cpu_idx];

    // Acking before the flush is safe because IRETQ is serialising: anything
    // that runs on this CPU afterwards sees the flush completed.
    let (op, asid) = {
        let mut queue = slot.queue.lock();
        let taken = queue.take();
        slot.ack.store(true, Ordering::Release);
        taken
    };

    // Acked above regardless of the filter: the initiator's wait is bounded on
    // the flag, not on whether this CPU actually needed the flush.
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

/// Batches page flushes; at the invlpg threshold the batch degrades to a full flush.
pub struct TlbFlushBatch {
    pages: [VirtAddr; INVLPG_THRESHOLD],
    count: usize,
}

impl TlbFlushBatch {
    pub const fn new() -> Self {
        Self {
            pages: [VirtAddr::NULL; INVLPG_THRESHOLD],
            count: 0,
        }
    }

    /// Pages past the threshold are dropped; `finish` then flushes everything.
    pub fn add(&mut self, vaddr: VirtAddr) {
        if self.count < INVLPG_THRESHOLD {
            self.pages[self.count] = vaddr;
            self.count += 1;
        }
    }

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
