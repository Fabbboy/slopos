//! Active-pipe discovery and scanline sampling.
//!
//! Pure read-only helpers over the pipe registers: pick the pipe the firmware is
//! actually scanning, and sample its scanline counter so the watchdog can prove
//! the panel kept scanning across a repoint. Neither function writes a register.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::{self, Pipe};

/// The first enabled pipe driving live output, or `None` if every pipe is
/// disabled. Reads only.
pub fn find_active(mmio: &MmioRegion) -> Option<Pipe> {
    for pipe in Pipe::ALL {
        if mmio.read::<u32>(regs::pipe_conf(pipe)) & regs::PIPECONF_ENABLE != 0 {
            return Some(pipe);
        }
    }
    None
}

/// Current scanline counter (`PIPEDSL`) for `pipe`. Reads only.
pub fn scanline(mmio: &MmioRegion, pipe: Pipe) -> u32 {
    mmio.read::<u32>(regs::pipe_dsl(pipe))
}

/// Block until the pipe completes a vertical blank — the point at which an armed
/// `PLANE_SURF` update latches into the active scanout configuration.
///
/// Polls `PIPEDSL` (read-only) for the scanline counter to wrap high→low, the
/// unambiguous per-frame vblank marker, in 1 ms steps holding no lock. Bounded by
/// `VBLANK_TIMEOUT_MS` (one 60 Hz frame plus margin) so a stalled pipe returns
/// rather than hanging the probe.
///
/// This is the flip-completion primitive: issuing a second plane-group flip
/// before the previous one has latched races the vblank, and on Gen12 leaves the
/// plane reading the linear surface with the X-tile (512-byte) `PLANE_STRIDE`
/// unit instead of the linear (64-byte) one — an 8x vertical replication. A lone
/// flip to a surface latches cleanly; it is two flips inside one frame that fail.
/// Callers that flip twice in succession (a repoint then a present flip) must
/// wait here between them so each flip latches first.
pub fn wait_for_vblank(mmio: &MmioRegion, pipe: Pipe) {
    const VBLANK_TIMEOUT_MS: u32 = 25;
    let mut prev = scanline(mmio, pipe);
    let mut elapsed_ms = 0;
    while elapsed_ms < VBLANK_TIMEOUT_MS {
        crate::hpet::delay_ms(1);
        let cur = scanline(mmio, pipe);
        if cur < prev {
            // The counter wrapped to the top of a new frame: a vblank elapsed.
            return;
        }
        prev = cur;
        elapsed_ms += 1;
    }
}
