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
use slopos_utils::{klog_debug, klog_info};

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

/// Per-CPU TLB shootdown state.
/// Each CPU has its own state to track pending flush requests.
#[repr(C, align(64))] // Cache line aligned to prevent false sharing
struct PerCpuTlbState {
    /// Pending flush request: 0 = none, 1 = single page, 2 = range, 3 = full
    pending_type: AtomicU32,
    /// Start address for single page or range flush.
    flush_start: AtomicU64,
    /// End address for range flush (exclusive).
    flush_end: AtomicU64,
    /// Address space identifier (CR3 value) for targeted flush, or 0 for all.
    target_asid: AtomicU64,
    /// Process ID for targeted process flushes, INVALID_PROCESS_ID for broadcast.
    target_process_id: AtomicU32,
    /// Requested TLB generation for targeted process flushes.
    request_tlb_gen: AtomicU64,
    /// Acknowledgment flag: set by target CPU when flush is complete.
    ack: AtomicBool,
    /// True when CPU runs kernel/idle and can defer user TLB flushes.
    is_lazy: AtomicBool,
    /// Process currently loaded on this CPU (or INVALID_PROCESS_ID).
    current_process_id: AtomicU32,
}

impl PerCpuTlbState {
    const fn new() -> Self {
        Self {
            pending_type: AtomicU32::new(0),
            flush_start: AtomicU64::new(0),
            flush_end: AtomicU64::new(0),
            target_asid: AtomicU64::new(0),
            target_process_id: AtomicU32::new(INVALID_PROCESS_ID),
            request_tlb_gen: AtomicU64::new(0),
            ack: AtomicBool::new(false),
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

struct ProcessTlbInfo {
    cpumask: CpuMask,
    tlb_gen: AtomicU64,
    last_flushed_gen: [AtomicU64; MAX_CPUS],
}

impl ProcessTlbInfo {
    const fn new() -> Self {
        const INIT: AtomicU64 = AtomicU64::new(0);
        Self {
            cpumask: CpuMask::new(),
            tlb_gen: AtomicU64::new(1),
            last_flushed_gen: [INIT; MAX_CPUS],
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
    info.tlb_gen.store(1, Ordering::Release);
    for local_gen in &info.last_flushed_gen {
        local_gen.store(0, Ordering::Release);
    }
}

pub fn unregister_process_tlb(process_id: u32) {
    let Some(info) = process_tlb_info(process_id) else {
        return;
    };

    info.cpumask.clear_all();
    info.tlb_gen.store(1, Ordering::Release);
    for local_gen in &info.last_flushed_gen {
        local_gen.store(0, Ordering::Release);
    }
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

fn send_shootdown_ipi() {
    let sender_ptr = IPI_SENDER.load(Ordering::Acquire);
    if sender_ptr.is_null() {
        return;
    }

    let sender: SendIpiFn = unsafe { core::mem::transmute(sender_ptr) };
    sender(TLB_SHOOTDOWN_VECTOR);
}

fn send_shootdown_ipi_to_cpu(cpu_idx: usize) {
    let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(cpu_idx) else {
        return;
    };
    slopos_arch::pcr::send_ipi_to_cpu(apic_id, TLB_SHOOTDOWN_VECTOR);
}

#[cfg(debug_assertions)]
static ACK_SPIN_WARNED: [AtomicBool; MAX_CPUS] = {
    const INIT: AtomicBool = AtomicBool::new(false);
    [INIT; MAX_CPUS]
};

fn wait_for_acks(targets: &[usize], initiator_cpu: usize) {
    // SYSCALL entry clears IF (SFMASK bit 9) so callers often arrive
    // with interrupts disabled.  Re-enable them for the spin-wait so
    // that (a) we can receive TLB IPIs from other CPUs that need our
    // ack, and (b) mouse/keyboard/timer IRQs keep firing.  The syscall
    // path holds NO_PREEMPT, so no context switch will occur.
    let was_enabled = cpu::are_interrupts_enabled();
    if !was_enabled {
        cpu::enable_interrupts();
    }

    for cpu_idx in targets {
        if *cpu_idx >= MAX_CPUS || *cpu_idx == initiator_cpu {
            continue;
        }

        let mut spin_count: u64 = 0;
        while !TLB_STATE.cpu_state[*cpu_idx].ack.load(Ordering::Acquire) {
            spin_count = spin_count.wrapping_add(1);
            #[cfg(debug_assertions)]
            if spin_count == 100_000_000
                && ACK_SPIN_WARNED[*cpu_idx]
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                klog_info!(
                    "TLB: long ack spin detected for CPU {} (possible shootdown bug)",
                    cpu_idx
                );
            }
            cpu::pause();
        }

        TLB_STATE.cpu_state[*cpu_idx]
            .ack
            .store(false, Ordering::Release);
    }

    if !was_enabled {
        cpu::disable_interrupts();
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

    let process_id = state.current_process_id.load(Ordering::Acquire);
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    let Some(info) = process_tlb_info(process_id) else {
        return;
    };

    let local_gen = info.last_flushed_gen[cpu].load(Ordering::Acquire);
    let global_gen = info.tlb_gen.load(Ordering::Acquire);
    if local_gen < global_gen {
        flush_tlb_local_full();
        info.last_flushed_gen[cpu].store(global_gen, Ordering::Release);
    }
}

fn queue_request_for_cpu(
    cpu_idx: usize,
    flush_type: FlushType,
    start: u64,
    end: u64,
    asid: u64,
    process_id: u32,
    tlb_gen: u64,
) {
    let state = &TLB_STATE.cpu_state[cpu_idx];
    state.ack.store(false, Ordering::Release);
    state.flush_start.store(start, Ordering::Release);
    state.flush_end.store(end, Ordering::Release);
    state.target_asid.store(asid, Ordering::Release);
    state.target_process_id.store(process_id, Ordering::Release);
    state.request_tlb_gen.store(tlb_gen, Ordering::Release);
    state
        .pending_type
        .store(flush_type as u32, Ordering::Release);
}

fn broadcast_flush_request(flush_type: FlushType, start: u64, end: u64, asid: u64) {
    let flush_type = promote_remote_flush_type(flush_type, start, end);

    for cpu_idx in TLB_STATE.online_cpus.iter_set() {
        queue_request_for_cpu(cpu_idx, flush_type, start, end, asid, INVALID_PROCESS_ID, 0);
    }

    core::sync::atomic::fence(Ordering::SeqCst);
}

fn targeted_flush_request(process_id: u32, flush_type: FlushType, start: u64, end: u64) {
    let Some(info) = process_tlb_info(process_id) else {
        return;
    };

    let flush_type = promote_remote_flush_type(flush_type, start, end);
    let request_gen = info.tlb_gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let initiator = slopos_arch::pcr::get_current_cpu();

    if should_flush_tlb_for_process(initiator, process_id) {
        match flush_type {
            FlushType::SinglePage => flush_page_local(VirtAddr::new(start)),
            FlushType::Range => flush_range_local(VirtAddr::new(start), VirtAddr::new(end)),
            FlushType::Full => flush_tlb_local_full(),
            FlushType::None => {}
        }
        info.last_flushed_gen[initiator].store(request_gen, Ordering::Release);
    }

    let mut targets = [0usize; MAX_CPUS];
    let mut target_count = 0usize;

    for cpu_idx in info.cpumask.iter_set() {
        if cpu_idx == initiator {
            continue;
        }
        if !slopos_arch::pcr::is_cpu_online(cpu_idx) || !should_flush_tlb(cpu_idx) {
            continue;
        }

        queue_request_for_cpu(cpu_idx, flush_type, start, end, 0, process_id, request_gen);
        if target_count < MAX_CPUS {
            targets[target_count] = cpu_idx;
            target_count += 1;
        }
    }

    if target_count == 0 {
        return;
    }

    core::sync::atomic::fence(Ordering::SeqCst);
    for cpu_idx in targets.iter().take(target_count) {
        send_shootdown_ipi_to_cpu(*cpu_idx);
    }
    wait_for_acks(&targets[..target_count], initiator);
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

fn online_cpu_targets(exclude: usize) -> ([usize; MAX_CPUS], usize) {
    let mut targets = [0usize; MAX_CPUS];
    let mut count = 0;
    for cpu_idx in TLB_STATE.online_cpus.iter_set() {
        if cpu_idx != exclude {
            targets[count] = cpu_idx;
            count += 1;
        }
    }
    (targets, count)
}

/// Flush a single page from all CPUs' TLBs.
///
/// This is the primary function called after unmapping a page.
/// On uniprocessor systems, it performs a local invlpg.
/// On SMP systems, it broadcasts an IPI to all CPUs.
pub fn flush_page(vaddr: VirtAddr) {
    flush_page_local(vaddr);

    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::SinglePage, vaddr.as_u64(), 0, 0);
        send_shootdown_ipi();
        let (targets, count) = online_cpu_targets(initiator);
        wait_for_acks(&targets[..count], initiator);
    }
}

pub fn flush_page_for_process(process_id: u32, vaddr: VirtAddr) {
    targeted_flush_request(process_id, FlushType::SinglePage, vaddr.as_u64(), 0);
}

/// Flush a range of pages from all CPUs' TLBs.
///
/// For small ranges, invalidates each page individually.
/// For large ranges, performs a full TLB flush.
pub fn flush_range(start: VirtAddr, end: VirtAddr) {
    flush_range_local(start, end);

    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Range, start.as_u64(), end.as_u64(), 0);
        send_shootdown_ipi();
        let (targets, count) = online_cpu_targets(initiator);
        wait_for_acks(&targets[..count], initiator);
    }
}

pub fn flush_range_for_process(process_id: u32, start: VirtAddr, end: VirtAddr) {
    targeted_flush_request(process_id, FlushType::Range, start.as_u64(), end.as_u64());
}

/// Flush the entire TLB on all CPUs.
///
/// This is the most expensive operation but sometimes necessary,
/// e.g., when changing CR3 or modifying many pages.
pub fn flush_all() {
    flush_tlb_local_full();

    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, 0);
        send_shootdown_ipi();
        let (targets, count) = online_cpu_targets(initiator);
        wait_for_acks(&targets[..count], initiator);
    }
}

pub fn flush_all_for_process(process_id: u32) {
    targeted_flush_request(process_id, FlushType::Full, 0, 0);
}

/// Flush TLB entries for a specific address space (ASID/CR3) on all CPUs.
///
/// This is useful when destroying a process - we only need to flush
/// entries associated with that process's page tables.
pub fn flush_asid(asid: u64) {
    let current_cr3 = cpu::read_cr3();

    if (current_cr3 & !0xFFF) == (asid & !0xFFF) {
        flush_tlb_local_full();
    }

    if is_smp_active() {
        let initiator = slopos_arch::pcr::get_current_cpu();
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, asid);
        send_shootdown_ipi();
        let (targets, count) = online_cpu_targets(initiator);
        wait_for_acks(&targets[..count], initiator);
    }
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

    let state = &TLB_STATE.cpu_state[cpu_idx];

    let flush_type = FlushType::from(state.pending_type.load(Ordering::Acquire));
    let start = state.flush_start.load(Ordering::Acquire);
    let end = state.flush_end.load(Ordering::Acquire);
    let target_asid = state.target_asid.load(Ordering::Acquire);
    let target_process_id = state.target_process_id.load(Ordering::Acquire);
    let request_tlb_gen = state.request_tlb_gen.load(Ordering::Acquire);

    state.pending_type.store(0, Ordering::Release);

    if target_asid != 0 {
        let local_cr3 = cpu::read_cr3();
        if (local_cr3 & !0xFFF) != (target_asid & !0xFFF) {
            state.ack.store(true, Ordering::Release);
            return;
        }
    }

    let mut do_full_flush = false;
    let mut skip_flush = false;

    if target_process_id != INVALID_PROCESS_ID {
        if let Some(info) = process_tlb_info(target_process_id) {
            let local_gen = info.last_flushed_gen[cpu_idx].load(Ordering::Acquire);
            if local_gen >= request_tlb_gen {
                skip_flush = true;
            } else {
                if request_tlb_gen.wrapping_sub(local_gen) > 1 {
                    do_full_flush = true;
                }
                info.last_flushed_gen[cpu_idx].store(request_tlb_gen, Ordering::Release);
            }
        }
    }

    if !skip_flush {
        if do_full_flush {
            flush_tlb_local_full();
        } else {
            match flush_type {
                FlushType::None => {}
                FlushType::SinglePage => {
                    flush_page_local(VirtAddr::new(start));
                }
                FlushType::Range => {
                    flush_range_local(VirtAddr::new(start), VirtAddr::new(end));
                }
                FlushType::Full => {
                    flush_tlb_local_full();
                }
            }
        }
    }

    state.ack.store(true, Ordering::Release);
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
