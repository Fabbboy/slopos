//! Primary-plane read-back and the linear repoint program.

use slopos_mm::mmio::MmioRegion;

use crate::xe_logic::plane_config::{self, PlaneConfig};
use crate::xe_logic::regs::{self, Pipe};

/// Decode the active pipe's live primary-plane configuration. Reads only.
pub fn read_live(mmio: &MmioRegion, pipe: Pipe) -> PlaneConfig {
    let ctl = mmio.read::<u32>(regs::plane_ctl(pipe));
    let size = mmio.read::<u32>(regs::plane_size(pipe));
    let pos = mmio.read::<u32>(regs::plane_pos(pipe));
    let stride = mmio.read::<u32>(regs::plane_stride(pipe));
    let surf = mmio.read::<u32>(regs::plane_surf(pipe));
    PlaneConfig::from_registers(ctl, size, pos, stride, surf)
}

/// The plane-group program for our linear scanout surface, bundled so a flip
/// can be re-issued every frame from one value.
///
/// Geometry and format only: the firmware's watermark/DDB (`PLANE_BUF_CFG` /
/// `PLANE_WM`), colour (`PLANE_COLOR_CTL`) and colour-key state for this mode
/// stays valid because the geometry/format never changes.
#[derive(Clone, Copy)]
pub struct PlaneProgram {
    pub pipe: Pipe,
    pub width: u32,
    pub height: u32,
    pub pitch_bytes: u32,
    pub format: plane_config::PlaneFormat,
    pub color_order: plane_config::ColorOrder,
}

impl PlaneProgram {
    /// Flip the plane to `surf_ggtt`, binding the stored geometry to
    /// [`program_repoint`].
    pub fn flip(&self, mmio: &MmioRegion, surf_ggtt: u32) {
        program_repoint(
            mmio,
            self.pipe,
            surf_ggtt,
            self.width,
            self.height,
            self.pitch_bytes,
            self.format,
            self.color_order,
        );
    }
}

/// Re-point the active plane at a kernel-owned linear, uncompressed framebuffer
/// at GGTT byte address `surf_ggtt`. A bare `PLANE_SURF` write is insufficient
/// on Gen12; the full plane group must be re-issued.
///
/// Writes only the active pipe's plane group, in arm order, with `PLANE_SURF`
/// last — that write latches the whole group atomically. `PLANE_COLOR_CTL` and
/// every pipe/transcoder/DDI/PLL/power-well register are left as firmware
/// programmed them.
pub fn program_repoint(
    mmio: &MmioRegion,
    pipe: Pipe,
    surf_ggtt: u32,
    width: u32,
    height: u32,
    pitch_bytes: u32,
    format: plane_config::PlaneFormat,
    color_order: plane_config::ColorOrder,
) {
    // The inherited firmware modeset scans full-screen, so the target is (0, 0).
    mmio.write::<u32>(regs::plane_pos(pipe), plane_config::encode_pos(0, 0));
    mmio.write::<u32>(
        regs::plane_size(pipe),
        plane_config::encode_size(width, height),
    );
    mmio.write::<u32>(
        regs::plane_stride(pipe),
        plane_config::linear_stride_reg(pitch_bytes),
    );
    // Linear has no tile/CCS offset; zeroing the AUX pair disables the
    // render-compression control surface entirely.
    mmio.write::<u32>(regs::plane_offset(pipe), 0);
    mmio.write::<u32>(regs::plane_aux_dist(pipe), 0);
    mmio.write::<u32>(regs::plane_aux_offset(pipe), 0);
    mmio.write::<u32>(
        regs::plane_ctl(pipe),
        plane_config::encode_ctl_linear(format, color_order, true),
    );
    mmio.write::<u32>(regs::plane_surf(pipe), surf_ggtt);
}
