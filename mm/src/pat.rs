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

use slopos_arch::cpu;
use slopos_arch::cpu::cpuid::{CPUID_FEAT_EDX_PAT, CPUID_LEAF_FEATURES};
use slopos_arch::cpu::msr::Msr;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::{klog_debug, klog_info};

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

static PAT_INIT: InitFlag = InitFlag::new();
static PAT_SUPPORTED: InitFlag = InitFlag::new();

#[inline]
pub fn is_initialized() -> bool {
    PAT_INIT.is_set()
}

#[inline]
pub fn is_supported() -> bool {
    PAT_SUPPORTED.is_set()
}

// =============================================================================
// PAT Detection
// =============================================================================

/// Check if PAT is supported by the CPU.
///
/// Reads CPUID leaf 1 and checks EDX bit 16.
pub fn pat_supported() -> bool {
    let (_, _, _, edx) = cpu::cpuid(CPUID_LEAF_FEATURES);
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
    if !PAT_INIT.init_once() {
        klog_debug!("PAT: Already initialized, skipping");
        return;
    }

    if !pat_supported() {
        panic!("PAT: Not supported by CPU - SlopOS requires PAT for framebuffer performance");
    }

    PAT_SUPPORTED.mark_set();

    klog_debug!("PAT: Initializing Page Attribute Table with WC support");

    let old_pat = cpu::read_msr(Msr::PAT);
    klog_debug!("PAT: Current value: 0x{:016x}", old_pat);

    let flags = cpu::save_flags_cli();

    cpu::wbinvd();
    cpu::flush_tlb_all();

    let cr0 = cpu::read_cr0();
    cpu::write_cr0((cr0 | cpu::CR0_CD) & !cpu::CR0_NW);

    cpu::wbinvd();
    cpu::write_msr(Msr::PAT, PAT_VALUE);
    cpu::wbinvd();

    cpu::write_cr0(cr0 & !cpu::CR0_CD & !cpu::CR0_NW);
    cpu::flush_tlb_all();

    cpu::restore_flags(flags);

    let new_pat = cpu::read_msr(Msr::PAT);
    if new_pat != PAT_VALUE {
        panic!(
            "PAT: Write verification failed! Expected {:#018x}, got {:#018x}",
            PAT_VALUE, new_pat
        );
    }
    klog_info!("PAT: Initialized with WC support (PA1=WC, PA5=WC)");
}
