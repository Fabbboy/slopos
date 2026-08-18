//! Address-space layout randomization: shifts the stack and heap bases of a
//! process memory layout by an RDRAND-derived, page-granular offset.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::memory_layout_defs::ProcessMemoryLayout;
use crate::paging_defs::PAGE_SIZE_4KB;
use slopos_arch::tsc;

#[derive(Clone, Copy)]
pub struct AslrConfig {
    pub stack_entropy_bits: u8,
    pub heap_entropy_bits: u8,
    pub enabled: bool,
}

impl AslrConfig {
    pub const fn default_config() -> Self {
        Self {
            stack_entropy_bits: 8,
            heap_entropy_bits: 12,
            enabled: true,
        }
    }

    pub const fn disabled() -> Self {
        Self {
            stack_entropy_bits: 0,
            heap_entropy_bits: 0,
            enabled: false,
        }
    }

    /// Bits 0..8 = stack_entropy_bits, 8..16 = heap_entropy_bits,
    /// 16 = enabled flag, 17..32 = reserved (must be zero).
    #[inline(always)]
    const fn pack(self) -> u32 {
        (self.stack_entropy_bits as u32)
            | ((self.heap_entropy_bits as u32) << 8)
            | ((self.enabled as u32) << 16)
    }

    #[inline(always)]
    const fn unpack(packed: u32) -> Self {
        Self {
            stack_entropy_bits: (packed & 0xFF) as u8,
            heap_entropy_bits: ((packed >> 8) & 0xFF) as u8,
            enabled: (packed & ENABLED_BIT) != 0,
        }
    }
}

impl Default for AslrConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

const ENABLED_BIT: u32 = 1 << 16;

/// All knobs in one `AtomicU32` so `get_config` is a single load: three
/// chained non-inlined `Atomic::load` calls each reserve a stack frame in
/// dev builds.
static ASLR_CONFIG: AtomicU32 = AtomicU32::new(AslrConfig::default_config().pack());

#[inline(always)]
pub fn get_config() -> AslrConfig {
    AslrConfig::unpack(ASLR_CONFIG.load(Ordering::Relaxed))
}

pub fn set_enabled(enabled: bool) {
    if enabled {
        ASLR_CONFIG.fetch_or(ENABLED_BIT, Ordering::Relaxed);
    } else {
        ASLR_CONFIG.fetch_and(!ENABLED_BIT, Ordering::Relaxed);
    }
}

#[inline(always)]
pub fn is_enabled() -> bool {
    (ASLR_CONFIG.load(Ordering::Relaxed) & ENABLED_BIT) != 0
}

fn get_random() -> u64 {
    if let Some(val) = slopos_arch::cpu::rdrand::RdRand::probe().and_then(|rd| rd.next()) {
        return val;
    }
    let a = tsc::rdtsc();
    let b = tsc::rdtsc();
    a.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(b)
}

pub fn randomize_layout(base: &ProcessMemoryLayout) -> ProcessMemoryLayout {
    let config = get_config();

    if !config.enabled {
        return *base;
    }

    let mut layout = *base;

    let stack_random = get_random();
    let heap_random = get_random();

    if config.stack_entropy_bits > 0 {
        let stack_mask = (1u64 << config.stack_entropy_bits) - 1;
        let stack_offset = (stack_random & stack_mask) * PAGE_SIZE_4KB;

        let min_stack_top = base.heap_max + base.stack_size + PAGE_SIZE_4KB;
        let new_stack_top = base.stack_top.saturating_sub(stack_offset);

        if new_stack_top > min_stack_top {
            layout.stack_top = new_stack_top;
        }
    }

    if config.heap_entropy_bits > 0 {
        let heap_mask = (1u64 << config.heap_entropy_bits) - 1;
        let heap_offset = (heap_random & heap_mask) * PAGE_SIZE_4KB;

        let max_heap_start = base.heap_max.saturating_sub(0x1000_0000);
        let new_heap_start = base.heap_start.saturating_add(heap_offset);

        if new_heap_start < max_heap_start {
            layout.heap_start = new_heap_start;
        }
    }

    layout
}

pub fn randomize_process_layout(base: &ProcessMemoryLayout) -> ProcessMemoryLayout {
    randomize_layout(base)
}
