//! Per-CPU memory-type census.
//!
//! `IA32_PAT` and the MTRRs are per-CPU MSRs, so a kernel that programs them on
//! the BSP alone leaves every AP on the firmware default, where PA1 is
//! Write-Through rather than Write-Combining.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use slopos_arch::MAX_CPUS;
use slopos_arch::cpu::msr::{self, Msr};

/// `IA32_MTRR_DEF_TYPE` — [11] MTRR enable, [10] fixed-range enable, [7:0]
/// default memory type.
const MSR_MTRR_DEF_TYPE: Msr = Msr::new(0x2FF);

/// The framebuffer PTE carries PWT=1, which selects PAT index 1.
pub const FB_PAT_INDEX: u8 = 1;

pub fn pat_entry(pat: u64, index: u8) -> u8 {
    ((pat >> (index as u32 * 8)) & 0xFF) as u8
}

pub fn memory_type_name(mem_type: u8) -> &'static str {
    match mem_type {
        0x00 => "UC",
        0x01 => "WC",
        0x04 => "WT",
        0x05 => "WP",
        0x06 => "WB",
        0x07 => "UC-",
        _ => "??",
    }
}

/// `recorded` discriminates a CPU that never ticked from one that read zero.
struct CpuEntry {
    pat: AtomicU64,
    mtrr_def_type: AtomicU64,
    cr0: AtomicU64,
    cr4: AtomicU64,
    recorded: AtomicU8,
}

impl CpuEntry {
    const fn new() -> Self {
        Self {
            pat: AtomicU64::new(0),
            mtrr_def_type: AtomicU64::new(0),
            cr0: AtomicU64::new(0),
            cr4: AtomicU64::new(0),
            recorded: AtomicU8::new(0),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_ENTRY: CpuEntry = CpuEntry::new();
static CENSUS: [CpuEntry; MAX_CPUS] = [EMPTY_ENTRY; MAX_CPUS];

/// Record the calling CPU's memory-type registers.
///
/// Runs on the timer tick, so it takes no lock: each slot has one writer and the
/// reader wants a recent value rather than a synchronized one.
pub fn record_current_cpu() {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu >= MAX_CPUS {
        return;
    }
    let entry = &CENSUS[cpu];

    // These registers do not change after AP bringup, so one sample is the
    // whole answer and later ticks skip the rdmsrs.
    if entry.recorded.load(Ordering::Relaxed) != 0 {
        return;
    }

    entry.pat.store(msr::read_msr(Msr::PAT), Ordering::Relaxed);
    entry.cr0.store(
        slopos_arch::cpu::control_regs::read_cr0(),
        Ordering::Relaxed,
    );
    entry.cr4.store(
        slopos_arch::cpu::control_regs::read_cr4(),
        Ordering::Relaxed,
    );

    // Reading MTRR_DEF_TYPE on a CPU without MTRR support is a #GP.
    let mtrr = if mtrr_supported() {
        msr::read_msr(MSR_MTRR_DEF_TYPE)
    } else {
        u64::MAX
    };
    entry.mtrr_def_type.store(mtrr, Ordering::Relaxed);

    entry.recorded.store(1, Ordering::Relaxed);
}

fn mtrr_supported() -> bool {
    let (_, _, _, edx) = slopos_arch::cpu::cpuid(1);
    (edx & (1 << 12)) != 0
}

pub struct CpuCacheState {
    pub pat: u64,
    pub mtrr_def_type: u64,
    pub cr0: u64,
    pub cr4: u64,
}

pub fn cpu_state(cpu: usize) -> Option<CpuCacheState> {
    let entry = CENSUS.get(cpu)?;
    if entry.recorded.load(Ordering::Relaxed) == 0 {
        return None;
    }
    Some(CpuCacheState {
        pat: entry.pat.load(Ordering::Relaxed),
        mtrr_def_type: entry.mtrr_def_type.load(Ordering::Relaxed),
        cr0: entry.cr0.load(Ordering::Relaxed),
        cr4: entry.cr4.load(Ordering::Relaxed),
    })
}

pub fn expected_pat() -> u64 {
    crate::pat::PAT_VALUE
}
