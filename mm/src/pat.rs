//! Page Attribute Table (PAT) configuration for x86-64.
//!
//! WC sits at index 1 (PAT=0, PCD=0, PWT=1), which is the `WRITE_THROUGH` page
//! flag with the PAT bit clear, so framebuffer writes combine instead of
//! going Write-Back.

use slopos_arch::cpu;
use slopos_arch::cpu::cpuid::{CPUID_FEAT_EDX_PAT, CPUID_LEAF_FEATURES};
use slopos_arch::cpu::msr::Msr;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::{klog_debug, klog_info};

/// Uncacheable - all accesses go directly to memory.
pub const MEM_TYPE_UC: u8 = 0x00;

/// Write-Combining - writes are buffered and combined into bursts.
pub const MEM_TYPE_WC: u8 = 0x01;

/// Write-Through - writes go to cache and memory simultaneously.
pub const MEM_TYPE_WT: u8 = 0x04;

/// Write-Protected - reads allocate cache lines, writes go to memory.
pub const MEM_TYPE_WP: u8 = 0x05;

/// Write-Back - normal caching, reads and writes use cache.
pub const MEM_TYPE_WB: u8 = 0x06;

/// Uncached (UC-) - like UC but can be overridden by MTRRs.
pub const MEM_TYPE_UC_MINUS: u8 = 0x07;

/// WC replaces the architectural default's WT entries at PA1 and PA5; the
/// resulting layout follows Linux's.
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

pub fn pat_supported() -> bool {
    let (_, _, _, edx) = cpu::cpuid(CPUID_LEAF_FEATURES);
    (edx & CPUID_FEAT_EDX_PAT) != 0
}

/// The cache-disable / WBINVD / TLB-flush sequence below is the Intel SDM's
/// mandated PAT-change procedure; its ordering is not optional.
///
/// # Safety
///
/// This function must be called:
/// - Early in boot, before any memory is mapped with WC
/// - Only once (subsequent calls are no-ops)
/// - With interrupts that might access memory disabled
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
