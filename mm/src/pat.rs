//! Page Attribute Table (PAT) configuration for x86-64.
//!
//! The PAT allows per-page memory type selection beyond what MTRRs provide.
//! This is critical for framebuffer performance - using Write-Combining (WC)
//! instead of Write-Back (WB) can yield 10-200x improvements for sequential
//! writes to video memory.
//!
//! # PAT Layout
//!
//! After initialization, the PAT entries are configured as:
//!
//! | Entry | Index (PAT:PCD:PWT) | Memory Type |
//! |-------|---------------------|-------------|
//! | PA0   | 000                 | WB (0x06)   |
//! | PA1   | 001                 | WC (0x01)   |
//! | PA2   | 010                 | UC- (0x07)  |
//! | PA3   | 011                 | UC (0x00)   |
//! | PA4   | 100                 | WB (0x06)   |
//! | PA5   | 101                 | WC (0x01)   |
//! | PA6   | 110                 | UC- (0x07)  |
//! | PA7   | 111                 | UC (0x00)   |
//!
//! This layout is Linux-compatible and places WC at index 1 (PWT=1, PCD=0, PAT=0),
//! which corresponds to the `WRITE_THROUGH` page flag when PAT bit is clear.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::arch::x86_64::cpuid::CPUID_FEAT_EDX_PAT;
use slopos_abi::arch::x86_64::msr::Msr;
use slopos_lib::{cpu, klog_debug, klog_info, klog_warn};

// =============================================================================
// Memory Type Constants
// =============================================================================

/// Uncacheable - all accesses go directly to memory.
pub const MEM_TYPE_UC: u8 = 0x00;

/// Write-Combining - writes are buffered and combined into bursts.
/// Optimal for framebuffer and other sequential write patterns.
pub const MEM_TYPE_WC: u8 = 0x01;

/// Write-Through - writes go to cache and memory simultaneously.
pub const MEM_TYPE_WT: u8 = 0x04;

/// Write-Protected - reads allocate cache lines, writes go to memory.
pub const MEM_TYPE_WP: u8 = 0x05;

/// Write-Back - normal caching, reads and writes use cache.
pub const MEM_TYPE_WB: u8 = 0x06;

/// Uncached (UC-) - like UC but can be overridden by MTRRs.
pub const MEM_TYPE_UC_MINUS: u8 = 0x07;

// =============================================================================
// PAT Configuration
// =============================================================================

/// PAT value with WC at index 1 (Linux-compatible layout).
///
/// This replaces the default WT entries with WC:
/// - PA0 = WB  (000) - default for normal memory
/// - PA1 = WC  (001) - for framebuffer/MMIO (was WT in default)
/// - PA2 = UC- (010) - uncached minus
/// - PA3 = UC  (011) - uncached
/// - PA4 = WB  (100) - same as PA0
/// - PA5 = WC  (101) - same as PA1 (was WT in default)
/// - PA6 = UC- (110) - same as PA2
/// - PA7 = UC  (111) - same as PA3
const PAT_VALUE: u64 = (MEM_TYPE_WB as u64)
    | ((MEM_TYPE_WC as u64) << 8)
    | ((MEM_TYPE_UC_MINUS as u64) << 16)
    | ((MEM_TYPE_UC as u64) << 24)
    | ((MEM_TYPE_WB as u64) << 32)
    | ((MEM_TYPE_WC as u64) << 40)
    | ((MEM_TYPE_UC_MINUS as u64) << 48)
    | ((MEM_TYPE_UC as u64) << 56);

/// Default PAT value (as set by CPU reset).
/// Used for logging comparison.
#[allow(dead_code)]
const PAT_DEFAULT: u64 = (MEM_TYPE_WB as u64)
    | ((MEM_TYPE_WT as u64) << 8)
    | ((MEM_TYPE_UC_MINUS as u64) << 16)
    | ((MEM_TYPE_UC as u64) << 24)
    | ((MEM_TYPE_WB as u64) << 32)
    | ((MEM_TYPE_WT as u64) << 40)
    | ((MEM_TYPE_UC_MINUS as u64) << 48)
    | ((MEM_TYPE_UC as u64) << 56);

// =============================================================================
// State Tracking
// =============================================================================

static PAT_INITIALIZED: AtomicBool = AtomicBool::new(false);
static PAT_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Check if PAT has been initialized.
#[inline]
pub fn is_initialized() -> bool {
    PAT_INITIALIZED.load(Ordering::Acquire)
}

/// Check if PAT is supported by the CPU.
#[inline]
pub fn is_supported() -> bool {
    PAT_SUPPORTED.load(Ordering::Acquire)
}

// =============================================================================
// PAT Detection
// =============================================================================

/// Check if PAT is supported by the CPU.
///
/// Reads CPUID leaf 1 and checks EDX bit 16.
pub fn pat_supported() -> bool {
    let (_, _, _, edx) = cpu::cpuid(1);
    (edx & CPUID_FEAT_EDX_PAT) != 0
}

// =============================================================================
// PAT Initialization
// =============================================================================

/// Initialize the Page Attribute Table with WC support.
///
/// This function reprograms the PAT MSR to include Write-Combining (WC)
/// at index 1, enabling WC for pages marked with PWT=1, PCD=0, PAT=0.
///
/// # Safety
///
/// This function must be called:
/// - Early in boot, before any memory is mapped with WC
/// - Only once (subsequent calls are no-ops)
/// - With interrupts that might access memory disabled
///
/// # Procedure (Intel SDM specified)
///
/// 1. Disable interrupts
/// 2. Flush all caches (WBINVD)
/// 3. Flush TLBs (CR3 reload)
/// 4. Disable caching (set CD flag in CR0)
/// 5. Flush caches again (WBINVD)
/// 6. Write new PAT value
/// 7. Flush caches (WBINVD)
/// 8. Re-enable caching (clear CD in CR0)
/// 9. Flush TLBs
/// 10. Re-enable interrupts
pub fn pat_init() {
    // Don't initialize twice
    if PAT_INITIALIZED.swap(true, Ordering::SeqCst) {
        klog_debug!("PAT: Already initialized, skipping");
        return;
    }

    // Check CPU support
    if !pat_supported() {
        klog_warn!("PAT: Not supported by CPU, framebuffer performance may suffer");
        PAT_SUPPORTED.store(false, Ordering::Release);
        return;
    }

    PAT_SUPPORTED.store(true, Ordering::Release);

    klog_debug!("PAT: Initializing Page Attribute Table with WC support");

    let old_pat = cpu::read_msr(Msr::PAT.address());
    klog_debug!("PAT: Current value: 0x{:016x}", old_pat);

    let flags = cpu::save_flags_cli();

    cpu::wbinvd();
    cpu::flush_tlb_all();

    let cr0 = cpu::read_cr0();
    cpu::write_cr0((cr0 | cpu::CR0_CD) & !cpu::CR0_NW);

    cpu::wbinvd();
    cpu::write_msr(Msr::PAT.address(), PAT_VALUE);
    cpu::wbinvd();

    cpu::write_cr0(cr0 & !cpu::CR0_CD & !cpu::CR0_NW);
    cpu::flush_tlb_all();

    cpu::restore_flags(flags);

    let new_pat = cpu::read_msr(Msr::PAT.address());
    if new_pat != PAT_VALUE {
        klog_warn!(
            "PAT: Write verification failed! Expected 0x{:016x}, got 0x{:016x}",
            PAT_VALUE,
            new_pat
        );
    } else {
        klog_info!("PAT: Initialized with WC support (PA1=WC, PA5=WC)");
        klog_debug!("PAT: New value: 0x{:016x}", new_pat);
    }
}

/// Get a human-readable name for a memory type.
#[allow(dead_code)]
pub fn memory_type_name(mem_type: u8) -> &'static str {
    match mem_type {
        MEM_TYPE_UC => "UC",
        MEM_TYPE_WC => "WC",
        MEM_TYPE_WT => "WT",
        MEM_TYPE_WP => "WP",
        MEM_TYPE_WB => "WB",
        MEM_TYPE_UC_MINUS => "UC-",
        _ => "???",
    }
}

/// Dump current PAT configuration for debugging.
#[allow(dead_code)]
pub fn dump_pat() {
    if !pat_supported() {
        klog_info!("PAT: Not supported by CPU");
        return;
    }

    let pat = cpu::read_msr(Msr::PAT.address());
    klog_info!(
        "PAT: Current configuration (MSR 0x{:x}):",
        Msr::PAT.address()
    );
    klog_info!("PAT: Raw value: 0x{:016x}", pat);

    for i in 0..8u8 {
        let mem_type = ((pat >> (i * 8)) & 0xFF) as u8;
        let pat_bit = (i >> 2) & 1;
        let pcd_bit = (i >> 1) & 1;
        let pwt_bit = i & 1;
        klog_info!(
            "PAT:   PA{} (PAT={} PCD={} PWT={}) = {} (0x{:02x})",
            i,
            pat_bit,
            pcd_bit,
            pwt_bit,
            memory_type_name(mem_type),
            mem_type
        );
    }
}
