//! DDB allocation and watermark encoding for the Gen12 hardware cursor; the
//! sequencing half (`crate::xe::ddb`) turns these values into register writes.
//!
//! The cursor is a real plane in the display engine's DBUF/watermark model, so
//! arming it with a zero DDB allocation is an invalid pipe state that starves
//! the primary's fetch. Cursor blocks come off the TAIL of the pipe's
//! allocation, written before the cursor is armed.

use super::regs;

/// The inherited single-pipe modeset this driver targets allocates 32 blocks.
pub const CURSOR_DDB_BLOCKS: u32 = 32;

/// Level-0 cursor watermark blocks. Fixed and conservative — the exact value needs
/// a PCODE memory-latency mailbox this driver lacks — and strictly below
/// [`CURSOR_DDB_BLOCKS`]: a watermark >= the plane's DDB allocation is invalid.
pub const CURSOR_WM0_BLOCKS: u32 = 8;

/// A DDB allocation as a half-open block range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DdbEntry {
    pub start: u32,
    pub end: u32,
}

impl DdbEntry {
    /// Block count, or 0 for an empty or degenerate range.
    pub const fn blocks(self) -> u32 {
        if self.end <= self.start {
            0
        } else {
            self.end - self.start
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorDdbSplit {
    pub primary: DdbEntry,
    pub cursor: DdbEntry,
}

/// Decode a `PLANE_BUF_CFG` / `CUR_BUF_CFG` value. The register stores END as the
/// inclusive last block; a zero register means no allocation and decodes to `[0, 0)`.
pub const fn decode_buf_cfg(reg: u32) -> DdbEntry {
    if reg == 0 {
        return DdbEntry { start: 0, end: 0 };
    }
    let start = regs::reg_field_get(regs::DDB_BUF_START_MASK, reg);
    let end_inclusive = regs::reg_field_get(regs::DDB_BUF_END_MASK, reg);
    DdbEntry {
        start,
        end: end_inclusive + 1,
    }
}

/// Encode a `[start, end)` range, writing END as the inclusive last block. An
/// empty or degenerate range encodes to 0 (no allocation).
pub const fn encode_buf_cfg(entry: DdbEntry) -> u32 {
    if entry.end <= entry.start {
        return 0;
    }
    regs::reg_field_set(regs::DDB_BUF_START_MASK, entry.start)
        | regs::reg_field_set(regs::DDB_BUF_END_MASK, entry.end - 1)
}

/// Carve `cursor_blocks` off the TAIL of the primary's allocation.
///
/// Returns `None` unless the primary keeps strictly more blocks than it surrenders,
/// so a degenerate firmware allocation cannot starve the primary or invert a range.
pub const fn carve_cursor_ddb(primary: DdbEntry, cursor_blocks: u32) -> Option<CursorDdbSplit> {
    if primary.end <= primary.start {
        return None;
    }
    if primary.blocks() <= cursor_blocks * 2 {
        return None;
    }
    let split_at = primary.end - cursor_blocks;
    Some(CursorDdbSplit {
        primary: DdbEntry {
            start: primary.start,
            end: split_at,
        },
        cursor: DdbEntry {
            start: split_at,
            end: primary.end,
        },
    })
}

/// Encode a `PLANE_WM` / `CUR_WM` level; a disabled level encodes to 0.
pub const fn wm_value(enable: bool, ignore_lines: bool, lines: u32, blocks: u32) -> u32 {
    if !enable {
        return 0;
    }
    let mut value = regs::WM_ENABLE | regs::reg_field_set(regs::WM_BLOCKS_MASK, blocks);
    if ignore_lines {
        value |= regs::WM_IGNORE_LINES;
    } else {
        value |= regs::reg_field_set(regs::WM_LINES_MASK, lines);
    }
    value
}

/// Level-0 cursor watermark. Higher levels and the transition watermark stay
/// disabled; the display falls back to level 0.
pub const fn cursor_wm0() -> u32 {
    wm_value(true, true, 0, CURSOR_WM0_BLOCKS)
}
