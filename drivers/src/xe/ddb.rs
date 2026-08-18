//! Gen12 hardware-cursor DBUF/DDB + watermark programming.
//!
//! A zero cursor DBUF allocation (`CUR_BUF_CFG = 0`, `CUR_WM = 0`) is an invalid
//! pipe state: it starves the pipe's fetch, and the primary plane then decodes
//! its linear surface at the X-tile (512-byte) stride unit, replicated 8x
//! vertically.
//!
//! `PLANE_BUF_CFG` is a no-arm register — it latches on the plane's `PLANE_SURF`
//! flip — so the primary's shrink must latch before the cursor claims the
//! reclaimed tail.

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::klog_info;

use super::pipe;
use super::plane::PlaneProgram;
use crate::xe_logic::ddb::{self, CURSOR_DDB_BLOCKS};
use crate::xe_logic::regs::{self, Pipe};

/// Returns `false` WITHOUT writing any register when the primary's allocation is
/// too small to carve [`CURSOR_DDB_BLOCKS`] off its tail — the caller then keeps
/// the cursor disabled (a software cursor) rather than risk corrupting scanout.
pub fn program_cursor_ddb(
    mmio: &MmioRegion,
    pipe: Pipe,
    program: &PlaneProgram,
    surf_ggtt: u32,
) -> bool {
    let primary = ddb::decode_buf_cfg(mmio.read::<u32>(regs::plane_buf_cfg(pipe)));
    let Some(split) = ddb::carve_cursor_ddb(primary, CURSOR_DDB_BLOCKS) else {
        klog_info!(
            "XE-DDB: primary DDB [{}, {}) too small to carve {} cursor blocks",
            primary.start,
            primary.end,
            CURSOR_DDB_BLOCKS
        );
        return false;
    };

    klog_info!(
        "XE-DDB: primary [{}, {}) -> [{}, {}); cursor [{}, {}) (pipe {:?})",
        primary.start,
        primary.end,
        split.primary.start,
        split.primary.end,
        split.cursor.start,
        split.cursor.end,
        pipe
    );

    mmio.write::<u32>(
        regs::plane_buf_cfg(pipe),
        ddb::encode_buf_cfg(split.primary),
    );
    program.flip(mmio, surf_ggtt);
    pipe::wait_for_vblank(mmio, pipe);

    // Higher levels and the transition watermark stay zero; the display then
    // falls back to level 0.
    mmio.write::<u32>(regs::cur_buf_cfg(pipe), ddb::encode_buf_cfg(split.cursor));
    mmio.write::<u32>(regs::cur_wm(pipe, 0), ddb::cursor_wm0());
    for level in 1..regs::PLANE_WM_LEVELS {
        mmio.write::<u32>(regs::cur_wm(pipe, level), 0);
    }
    mmio.write::<u32>(regs::cur_wm_trans(pipe), 0);
    true
}
