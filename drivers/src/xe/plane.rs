//! Primary-plane read-back and the linear repoint program.
//!
//! The only display registers a repoint writes are the active pipe's plane
//! group — `PLANE_CTL`, `PLANE_STRIDE`, `PLANE_POS`, `PLANE_SIZE`,
//! `PLANE_OFFSET`, `PLANE_AUX_DIST`, `PLANE_AUX_OFFSET`, and `PLANE_SURF`. It
//! never writes `PLANE_COLOR_CTL` (firmware color management is preserved) and
//! never any pipe/transcoder/DDI/PLL/power-well register. `PLANE_SURF` is the
//! last write, arming the double-buffered group atomically.

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

/// The plane-group program for our linear scanout surface: pipe, geometry, and
/// format. Bundled so the stored present / flush state can re-issue a flip on
/// every frame from one value.
///
/// This carries ONLY geometry. The firmware already programmed correct
/// watermarks/DDB (`PLANE_BUF_CFG` / `PLANE_WM`), colour (`PLANE_COLOR_CTL`), and
/// colour-key state for this mode, and we keep the same geometry/format, so a
/// flip re-issues only the geometry group and leaves that inherited state
/// untouched: the no-arm registers, then the arming `PLANE_SURF`.
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
    /// Flip the plane to `surf_ggtt` with a synchronous geometry plane-group
    /// update — `PLANE_STRIDE` / `PLANE_CTL` (linear) and the rest, arming with
    /// `PLANE_SURF` last. Binds the stored geometry to [`program_repoint`]. The
    /// firmware watermark/DDB/colour/key state is inherited and left untouched.
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
/// at GGTT byte address `surf_ggtt`, arming the double-buffered plane group. This
/// is the canonical full plane-group flip — the non-arming registers, then
/// `PLANE_CTL`, then `PLANE_SURF` — and the only correct way to change the
/// scanned surface (see [`PlaneProgram::flip`] for why a bare `PLANE_SURF` write
/// is insufficient on Gen12).
///
/// Writes only the active pipe's plane group, in arm order: position at the
/// origin, the new size/stride, the offset
/// and `PLANE_AUX_*` pair zeroed to disable CCS/decompression, the linear
/// `PLANE_CTL` (render-decompression cleared, `PLANE_COLOR_CTL` untouched so
/// firmware color management is preserved), and finally `PLANE_SURF` — the last
/// write, which latches the whole group atomically. It never writes
/// `PLANE_COLOR_CTL` or any pipe/transcoder/PLL/power-well register.
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
    // Primary plane origin: the inherited firmware modeset scans a full-screen
    // plane, so the linear repoint target sits at (0, 0).
    mmio.write::<u32>(regs::plane_pos(pipe), plane_config::encode_pos(0, 0));
    mmio.write::<u32>(
        regs::plane_size(pipe),
        plane_config::encode_size(width, height),
    );
    mmio.write::<u32>(
        regs::plane_stride(pipe),
        plane_config::linear_stride_reg(pitch_bytes),
    );
    // A linear surface has no tile/CCS offset, and zeroing the AUX distance and
    // offset disables the render-compression control surface entirely.
    mmio.write::<u32>(regs::plane_offset(pipe), 0);
    mmio.write::<u32>(regs::plane_aux_dist(pipe), 0);
    mmio.write::<u32>(regs::plane_aux_offset(pipe), 0);
    // LINEAR tiling, render-decompression cleared, plane enabled. PLANE_COLOR_CTL
    // is deliberately left as the firmware programmed it.
    mmio.write::<u32>(
        regs::plane_ctl(pipe),
        plane_config::encode_ctl_linear(format, color_order, true),
    );
    // PLANE_SURF last: arms the double-buffered plane group atomically.
    mmio.write::<u32>(regs::plane_surf(pipe), surf_ggtt);
}
