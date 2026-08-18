//! Active-pipe discovery and scanline sampling.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::{self, Pipe};

/// The first enabled pipe driving live output, or `None` if every pipe is off.
pub fn find_active(mmio: &MmioRegion) -> Option<Pipe> {
    for pipe in Pipe::ALL {
        if mmio.read::<u32>(regs::pipe_conf(pipe)) & regs::PIPECONF_ENABLE != 0 {
            return Some(pipe);
        }
    }
    None
}

pub fn scanline(mmio: &MmioRegion, pipe: Pipe) -> u32 {
    mmio.read::<u32>(regs::pipe_dsl(pipe))
}

/// Block until the pipe completes a vertical blank — the point at which an armed
/// `PLANE_SURF` update latches — holding no lock. Bounded by `VBLANK_TIMEOUT_MS`
/// so a stalled pipe returns rather than hanging the probe.
///
/// Two plane-group flips inside one frame race the vblank: on Gen12 the plane
/// then reads a linear surface with the X-tile (512-byte) `PLANE_STRIDE` unit
/// instead of the linear (64-byte) one, an 8x vertical replication. Callers that
/// flip twice in succession must wait here between them.
pub fn wait_for_vblank(mmio: &MmioRegion, pipe: Pipe) {
    // One 60 Hz frame (~16.7 ms) plus margin.
    const VBLANK_TIMEOUT_MS: u32 = 25;
    let mut prev = scanline(mmio, pipe);
    let mut elapsed_ms = 0;
    while elapsed_ms < VBLANK_TIMEOUT_MS {
        crate::hpet::delay_ms(1);
        let cur = scanline(mmio, pipe);
        if cur < prev {
            return;
        }
        prev = cur;
        elapsed_ms += 1;
    }
}
