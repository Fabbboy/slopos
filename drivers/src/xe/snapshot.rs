//! Pre-write snapshot of every display register the repoint will touch.
//!
//! Before the first plane write, the active pipe's plane-group registers are
//! captured so [`restore`] can rewrite them — `PLANE_SURF` last — to put the
//! firmware framebuffer back if the repoint is declined or the watchdog rolls it
//! back. Capture is read-only; restore is the only writer here and never touches
//! `PLANE_COLOR_CTL` (firmware color management is preserved).

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::{self, Pipe};

/// Saved copy of the active pipe's plane-group registers — every register the
/// repoint is allowed to write — taken before the first write so the firmware
/// framebuffer can be put back verbatim.
#[derive(Clone, Copy, Debug)]
pub struct DisplaySnapshot {
    pub pipe: Pipe,
    pub plane_ctl: u32,
    pub plane_stride: u32,
    pub plane_pos: u32,
    pub plane_size: u32,
    pub plane_offset: u32,
    pub plane_aux_dist: u32,
    pub plane_aux_offset: u32,
    pub plane_surf: u32,
}

/// Read back the active pipe's plane-group registers into a [`DisplaySnapshot`].
/// Read-only: captures the firmware state before any write.
pub fn capture(mmio: &MmioRegion, pipe: Pipe) -> DisplaySnapshot {
    DisplaySnapshot {
        pipe,
        plane_ctl: mmio.read::<u32>(regs::plane_ctl(pipe)),
        plane_stride: mmio.read::<u32>(regs::plane_stride(pipe)),
        plane_pos: mmio.read::<u32>(regs::plane_pos(pipe)),
        plane_size: mmio.read::<u32>(regs::plane_size(pipe)),
        plane_offset: mmio.read::<u32>(regs::plane_offset(pipe)),
        plane_aux_dist: mmio.read::<u32>(regs::plane_aux_dist(pipe)),
        plane_aux_offset: mmio.read::<u32>(regs::plane_aux_offset(pipe)),
        plane_surf: mmio.read::<u32>(regs::plane_surf(pipe)),
    }
}

/// Rewrite the snapshot's saved registers to restore the firmware framebuffer.
///
/// Writes every captured plane-group register back to the snapshot's pipe with
/// `PLANE_SURF` written LAST, so the double-buffered plane group re-arms the
/// firmware surface atomically. `PLANE_COLOR_CTL` is never written — the
/// firmware's color management is left exactly as found — and no
/// pipe/transcoder/DDI/PLL/power-well register is touched.
pub fn restore(mmio: &MmioRegion, snap: &DisplaySnapshot) {
    let pipe = snap.pipe;
    // Non-arming group registers first; they latch on the trailing PLANE_SURF.
    mmio.write::<u32>(regs::plane_ctl(pipe), snap.plane_ctl);
    mmio.write::<u32>(regs::plane_stride(pipe), snap.plane_stride);
    mmio.write::<u32>(regs::plane_pos(pipe), snap.plane_pos);
    mmio.write::<u32>(regs::plane_size(pipe), snap.plane_size);
    mmio.write::<u32>(regs::plane_offset(pipe), snap.plane_offset);
    mmio.write::<u32>(regs::plane_aux_dist(pipe), snap.plane_aux_dist);
    mmio.write::<u32>(regs::plane_aux_offset(pipe), snap.plane_aux_offset);
    // PLANE_SURF LAST: arms the double-buffered group atomically.
    mmio.write::<u32>(regs::plane_surf(pipe), snap.plane_surf);
}
