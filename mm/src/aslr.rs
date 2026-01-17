//! Address Space Layout Randomization (ASLR) for SlopOS.
//!
//! Randomizes stack (1MB range) and heap (16MB range) to mitigate exploitation.
//! Uses TSC-seeded LFSR64 RNG.

use crate::memory_layout::ProcessMemoryLayout;
use crate::mm_constants::PAGE_SIZE_4KB;
use slopos_lib::tsc;

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
}

impl Default for AslrConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

static mut ASLR_CONFIG: AslrConfig = AslrConfig::default_config();

pub fn get_config() -> AslrConfig {
    unsafe { ASLR_CONFIG }
}

pub fn set_enabled(enabled: bool) {
    unsafe {
        ASLR_CONFIG.enabled = enabled;
    }
}

pub fn is_enabled() -> bool {
    unsafe { ASLR_CONFIG.enabled }
}

fn get_random() -> u64 {
    let mut x = tsc::rdtsc() | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
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
