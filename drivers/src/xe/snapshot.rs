//! Pre-write snapshot of every display register the repoint will touch, so
//! [`restore`] can put the firmware framebuffer back when a repoint is declined
//! or the watchdog rolls it back.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::regs::{self, Pipe};

/// Saved copy of the active pipe's plane-group registers — every register the
/// repoint is allowed to write.
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

/// Capture the firmware's plane-group register state before any write.
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
/// `PLANE_SURF` goes LAST, so the double-buffered plane group re-arms the
/// firmware surface atomically. `PLANE_COLOR_CTL` and every
/// pipe/transcoder/DDI/PLL/power-well register are left as found.
pub fn restore(mmio: &MmioRegion, snap: &DisplaySnapshot) {
    let pipe = snap.pipe;
    mmio.write::<u32>(regs::plane_ctl(pipe), snap.plane_ctl);
    mmio.write::<u32>(regs::plane_stride(pipe), snap.plane_stride);
    mmio.write::<u32>(regs::plane_pos(pipe), snap.plane_pos);
    mmio.write::<u32>(regs::plane_size(pipe), snap.plane_size);
    mmio.write::<u32>(regs::plane_offset(pipe), snap.plane_offset);
    mmio.write::<u32>(regs::plane_aux_dist(pipe), snap.plane_aux_dist);
    mmio.write::<u32>(regs::plane_aux_offset(pipe), snap.plane_aux_offset);
    mmio.write::<u32>(regs::plane_surf(pipe), snap.plane_surf);
}
