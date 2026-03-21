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
use slopos_arch::cpu;
use slopos_arch::pcr::MAX_CPUS;
use slopos_utils::{klog_debug, klog_info};

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
    /// Acknowledgment flag: set by target CPU when flush is complete.
    ack: AtomicBool,
}

impl PerCpuTlbState {
    const fn new() -> Self {
        Self {
            pending_type: AtomicU32::new(0),
            flush_start: AtomicU64::new(0),
            flush_end: AtomicU64::new(0),
            target_asid: AtomicU64::new(0),
            ack: AtomicBool::new(false),
        }
    }
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
    /// Number of active CPUs (set during SMP bringup).
    active_cpu_count: AtomicU32,
    /// Global sequence number for ordering.
    sequence: AtomicU64,
}

impl TlbShootdownState {
    const fn new() -> Self {
        const INIT_STATE: PerCpuTlbState = PerCpuTlbState::new();
        Self {
            cpu_state: [INIT_STATE; MAX_CPUS],
            active_cpu_count: AtomicU32::new(1),
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
    TLB_STATE.active_cpu_count.load(Ordering::Relaxed) > 1
}

/// Get the current number of active CPUs.
#[inline]
pub fn get_active_cpu_count() -> u32 {
    TLB_STATE.active_cpu_count.load(Ordering::Relaxed)
}

/// Notify the TLB subsystem that a new CPU is online.
///
/// Called during AP startup after the CPU's topology has been registered
/// via `slopos_arch::pcr::init_ap_pcr`. This only updates the TLB
/// shootdown active-CPU count; topology lives in `slopos_arch::pcr`.
pub fn notify_cpu_online() {
    TLB_STATE.active_cpu_count.fetch_add(1, Ordering::AcqRel);
}

fn send_shootdown_ipi() {
    let sender_ptr = IPI_SENDER.load(Ordering::Acquire);
    if sender_ptr.is_null() {
        return;
    }

    let sender: SendIpiFn = unsafe { core::mem::transmute(sender_ptr) };
    sender(TLB_SHOOTDOWN_VECTOR);
}

fn wait_for_acks(initiator_cpu: usize) {
    let cpu_count = TLB_STATE.active_cpu_count.load(Ordering::Acquire) as usize;

    // SYSCALL entry clears IF (SFMASK bit 9) so callers often arrive
    // with interrupts disabled.  Re-enable them for the spin-wait so
    // that (a) we can receive TLB IPIs from other CPUs that need our
    // ack, and (b) mouse/keyboard/timer IRQs keep firing.  The syscall
    // path holds NO_PREEMPT, so no context switch will occur.
    let was_enabled = cpu::are_interrupts_enabled();
    if !was_enabled {
        cpu::enable_interrupts();
    }

    for cpu_idx in 0..cpu_count.min(MAX_CPUS) {
        if cpu_idx == initiator_cpu {
            continue;
        }

        let mut timeout = 1_000_000;
        while !TLB_STATE.cpu_state[cpu_idx].ack.load(Ordering::Acquire) && timeout > 0 {
            cpu::pause();
            timeout -= 1;
        }

        if timeout == 0 {
            klog_info!("TLB: Warning - CPU {} did not acknowledge flush", cpu_idx);
        }

        TLB_STATE.cpu_state[cpu_idx]
            .ack
            .store(false, Ordering::Release);
    }

    if !was_enabled {
        cpu::disable_interrupts();
    }
}

fn broadcast_flush_request(flush_type: FlushType, start: u64, end: u64, asid: u64) {
    let cpu_count = TLB_STATE.active_cpu_count.load(Ordering::Acquire) as usize;

    for cpu_idx in 0..cpu_count.min(MAX_CPUS) {
        TLB_STATE.cpu_state[cpu_idx]
            .flush_start
            .store(start, Ordering::Release);
        TLB_STATE.cpu_state[cpu_idx]
            .flush_end
            .store(end, Ordering::Release);
        TLB_STATE.cpu_state[cpu_idx]
            .target_asid
            .store(asid, Ordering::Release);
        TLB_STATE.cpu_state[cpu_idx]
            .pending_type
            .store(flush_type as u32, Ordering::Release);
    }

    core::sync::atomic::fence(Ordering::SeqCst);
}

// =============================================================================
// Public TLB Flush API
// =============================================================================

/// Initialize the TLB subsystem.
/// Called during kernel boot.
pub fn init() {
    detect_features();
    klog_info!("TLB: Subsystem initialized");
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
        wait_for_acks(initiator);
    }
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
        wait_for_acks(initiator);
    }
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
        wait_for_acks(initiator);
    }
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
        wait_for_acks(initiator);
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

    state.pending_type.store(0, Ordering::Release);

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
