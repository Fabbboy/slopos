//! Per-CPU framebuffer write-throughput census.
//!
//! Measured on the compositor's own blit rather than a synthetic buffer: a
//! benchmark over its own memory would not carry the framebuffer's memory type,
//! which is the quantity under test.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::MAX_CPUS;

struct CpuBlit {
    cycles: AtomicU64,
    bytes: AtomicU64,
    frames: AtomicU64,
}

impl CpuBlit {
    const fn new() -> Self {
        Self {
            cycles: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            frames: AtomicU64::new(0),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: CpuBlit = CpuBlit::new();
static BLIT: [CpuBlit; MAX_CPUS] = [EMPTY; MAX_CPUS];

/// Accumulate one blit's cost against the CPU that performed it.
///
/// `rdtsc` rather than the HPET because an uncached MMIO read would be a large
/// fraction of the span being measured. Cross-CPU TSC skew does not matter:
/// each slot is only ever read as a rate against itself.
pub fn record(start_tsc: u64, end_tsc: u64, bytes: usize) {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu >= MAX_CPUS {
        return;
    }
    let entry = &BLIT[cpu];
    entry
        .cycles
        .fetch_add(end_tsc.wrapping_sub(start_tsc), Ordering::Relaxed);
    entry.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    entry.frames.fetch_add(1, Ordering::Relaxed);
}

pub struct CpuBlitStats {
    pub cycles: u64,
    pub bytes: u64,
    pub frames: u64,
}

pub fn stats(cpu: usize) -> Option<CpuBlitStats> {
    let entry = BLIT.get(cpu)?;
    let frames = entry.frames.load(Ordering::Relaxed);
    if frames == 0 {
        return None;
    }
    Some(CpuBlitStats {
        cycles: entry.cycles.load(Ordering::Relaxed),
        bytes: entry.bytes.load(Ordering::Relaxed),
        frames,
    })
}

pub fn reset() {
    for entry in BLIT.iter() {
        entry.cycles.store(0, Ordering::Relaxed);
        entry.bytes.store(0, Ordering::Relaxed);
        entry.frames.store(0, Ordering::Relaxed);
    }
}
