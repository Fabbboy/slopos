//! Synchronous scan-out watchdog: confirms the panel is still scanning before a
//! repoint is committed, so a bad repoint never leaves a black screen. The poll
//! loop holds no `SpinLock` while delaying.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::Pipe;

/// Poll `PIPEDSL` for up to `wdog_ms` milliseconds in 1 ms steps and report
/// whether the scanline is still advancing.
///
/// `true` means the pipe is scanning out and the caller may commit the repoint;
/// `false` means it stalled or underran and the caller must roll back to the
/// firmware framebuffer. `wdog_ms` of `0` declines immediately.
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
