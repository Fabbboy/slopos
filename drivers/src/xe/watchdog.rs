//! Synchronous scan-out watchdog.
//!
//! After a repoint write, the watchdog confirms the panel is still scanning
//! before the install is committed; a failure makes the caller restore the
//! snapshot and decline, so a bad repoint never leaves a black screen. The poll
//! loop uses [`crate::hpet::delay_ms`] and holds no `SpinLock` while delaying.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::Pipe;

/// Poll the pipe scanline for up to `wdog_ms` milliseconds and report whether it
/// is still advancing (the panel is scanning out our new surface).
///
/// Samples [`super::pipe::scanline`] (`PIPEDSL`, read-only) against the value
/// captured before the loop, spinning [`crate::hpet::delay_ms`] in 1 ms steps
/// and holding no lock. Returns `true` the instant the scanline
/// differs from the baseline — the pipe is still actively scanning out, so the
/// caller may commit the repoint. Returns `false` if the scanline never advances
/// within the window — the pipe stalled or underran, so the caller rolls back to
/// the firmware framebuffer. A `wdog_ms` of `0` declines immediately (no window
/// to confirm in).
pub fn confirm_scanning(mmio: &MmioRegion, pipe: Pipe, wdog_ms: u32) -> bool {
    let baseline = super::pipe::scanline(mmio, pipe);
    let mut elapsed_ms = 0;
    while elapsed_ms < wdog_ms {
        crate::hpet::delay_ms(1);
        if super::pipe::scanline(mmio, pipe) != baseline {
            return true;
        }
        elapsed_ms += 1;
    }
    false
}
