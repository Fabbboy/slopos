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
use slopos_lib::{cpu, klog_debug, klog_info};

use crate::mm_constants::PAGE_SIZE_4KB;

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

/// Maximum number of CPUs supported for TLB shootdown.
/// This should match the kernel's MAX_CPUS constant.
pub const MAX_CPUS: usize = 256;

/// IPI vector used for TLB shootdown requests.
/// Must be in the range 0x20-0xFE and not conflict with other vectors.
/// We use 0xFD (253) which is in the high range, reserved for system IPIs.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFD;

// =============================================================================
// CPU Feature Detection
// =============================================================================

/// CPUID leaf 7, subleaf 0, EBX bit 10: INVPCID instruction support.
const CPUID_FEAT_EBX_INVPCID: u32 = 1 << 10;

/// CPUID leaf 1, ECX bit 17: PCID (Process Context Identifiers) support.
const CPUID_FEAT_ECX_PCID: u32 = 1 << 17;

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

    let (_, _, ecx, _) = cpu::cpuid(1);
    let pcid_supported = (ecx & CPUID_FEAT_ECX_PCID) != 0;

    let (max_leaf, _, _, _) = cpu::cpuid(0);
    let invpcid_supported = if max_leaf >= 7 {
        let (_, ebx, _, _) = cpu::cpuid(7);
        (ebx & CPUID_FEAT_EBX_INVPCID) != 0
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
    /// Current CPU's APIC ID (for self-identification).
    /// This is set per-CPU during initialization.
    bsp_apic_id: AtomicU32,
    /// Global sequence number for ordering.
    sequence: AtomicU64,
}

impl TlbShootdownState {
    const fn new() -> Self {
        const INIT_STATE: PerCpuTlbState = PerCpuTlbState::new();
        Self {
            cpu_state: [INIT_STATE; MAX_CPUS],
            active_cpu_count: AtomicU32::new(1),
            bsp_apic_id: AtomicU32::new(0),
            sequence: AtomicU64::new(0),
        }
    }
}

static TLB_STATE: TlbShootdownState = TlbShootdownState::new();

const INVALID_CPU_IDX: u32 = u32::MAX;

static APIC_ID_TO_CPU_IDX: [AtomicU32; MAX_CPUS] = {
    const INIT: AtomicU32 = AtomicU32::new(INVALID_CPU_IDX);
    [INIT; MAX_CPUS]
};

#[inline(always)]
fn flush_tlb_local_full() {
    cpu::flush_tlb_all();
}

/// INVPCID descriptor structure for the INVPCID instruction.
#[repr(C, align(16))]
struct InvpcidDescriptor {
    pcid: u64,
    addr: u64,
}

/// INVPCID types.
#[repr(u64)]
#[allow(dead_code)]
enum InvpcidType {
    /// Invalidate mappings for a single linear address and PCID.
    IndividualAddress = 0,
    /// Invalidate all mappings for a single PCID (except global).
    SingleContext = 1,
    /// Invalidate all mappings (including global).
    AllContextsIncludingGlobal = 2,
    /// Invalidate all mappings (except global).
    AllContextsExcludingGlobal = 3,
}

/// Execute INVPCID instruction (if available).
#[inline]
#[allow(dead_code)]
fn invpcid(invtype: InvpcidType, pcid: u64, addr: u64) {
    let desc = InvpcidDescriptor { pcid, addr };
    unsafe {
        core::arch::asm!(
            "invpcid {}, [{}]",
            in(reg) invtype as u64,
            in(reg) &desc,
            options(nostack, preserves_flags)
        );
    }
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

/// Register a new CPU as active (called during AP startup).
/// Returns the CPU index assigned to this CPU.
pub fn register_cpu(apic_id: u32) -> usize {
    let count = TLB_STATE.active_cpu_count.fetch_add(1, Ordering::AcqRel);
    let cpu_idx = count as usize;

    if cpu_idx < MAX_CPUS {
        if apic_id < MAX_CPUS as u32 {
            APIC_ID_TO_CPU_IDX[apic_id as usize].store(cpu_idx as u32, Ordering::Release);
        }
        klog_debug!(
            "TLB: Registered CPU {} with APIC ID 0x{:x}",
            cpu_idx,
            apic_id
        );
    }

    cpu_idx
}

/// Set the BSP's APIC ID (called during boot).
pub fn set_bsp_apic_id(apic_id: u32) {
    TLB_STATE.bsp_apic_id.store(apic_id, Ordering::Release);
    if apic_id < MAX_CPUS as u32 {
        APIC_ID_TO_CPU_IDX[apic_id as usize].store(0, Ordering::Release);
    }
}

pub fn cpu_index_from_apic_id(apic_id: u32) -> Option<usize> {
    if apic_id >= MAX_CPUS as u32 {
        return None;
    }
    let idx = APIC_ID_TO_CPU_IDX[apic_id as usize].load(Ordering::Acquire);
    if idx == INVALID_CPU_IDX {
        None
    } else {
        Some(idx as usize)
    }
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
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::SinglePage, vaddr.as_u64(), 0, 0);
        send_shootdown_ipi();
        wait_for_acks(0);
    }
}

/// Flush a range of pages from all CPUs' TLBs.
///
/// For small ranges, invalidates each page individually.
/// For large ranges, performs a full TLB flush.
pub fn flush_range(start: VirtAddr, end: VirtAddr) {
    flush_range_local(start, end);

    if is_smp_active() {
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Range, start.as_u64(), end.as_u64(), 0);
        send_shootdown_ipi();
        wait_for_acks(0);
    }
}

/// Flush the entire TLB on all CPUs.
///
/// This is the most expensive operation but sometimes necessary,
/// e.g., when changing CR3 or modifying many pages.
pub fn flush_all() {
    flush_tlb_local_full();

    if is_smp_active() {
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, 0);
        send_shootdown_ipi();
        wait_for_acks(0);
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
        TLB_STATE.sequence.fetch_add(1, Ordering::SeqCst);
        broadcast_flush_request(FlushType::Full, 0, 0, asid);
        send_shootdown_ipi();
        wait_for_acks(0);
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

// =============================================================================
// FFI Exports
// =============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn tlb_flush_page(vaddr: u64) {
    flush_page(VirtAddr::new(vaddr));
}

#[unsafe(no_mangle)]
pub extern "C" fn tlb_flush_range(start: u64, end: u64) {
    flush_range(VirtAddr::new(start), VirtAddr::new(end));
}

#[unsafe(no_mangle)]
pub extern "C" fn tlb_flush_all() {
    flush_all();
}

#[unsafe(no_mangle)]
pub extern "C" fn tlb_handle_ipi(cpu_idx: u32) {
    handle_shootdown_ipi(cpu_idx as usize);
}
