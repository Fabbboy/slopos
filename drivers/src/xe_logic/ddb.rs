//! Pure cursor DBUF/DDB allocation + watermark encoding for the Gen12 hardware
//! cursor.
//!
//! Plain block-index and bitfield math over `core` only — no MMIO, no allocation.
//! The hardware-sequencing half (`crate::xe::ddb`) supplies the register window
//! and turns these values into PLANE_BUF_CFG / CUR_BUF_CFG / CUR_WM writes.
//!
//! Why this exists: the Gen12 cursor is a real plane in the display engine's DBUF
//! (data-buffer) and watermark model, not a sideband overlay. Enabling it with a
//! zero DDB allocation is an invalid pipe state that starves the pipe's fetch, so
//! the PRIMARY plane decodes its linear surface with the X-tile (512-byte) stride
//! unit and replicates 8x vertically. The cure is to reserve cursor blocks from
//! the TAIL of the pipe's allocation and write the cursor DDB + watermark before
//! arming the cursor. This module is the pure arithmetic behind that: decode the
//! firmware's primary allocation, carve the cursor's tail blocks, and encode the
//! DDB/watermark register values.

use super::regs;

/// DDB blocks reserved for the cursor on a single active pipe. The inherited
/// single-pipe modeset this driver targets allocates 32 blocks to the cursor.
pub const CURSOR_DDB_BLOCKS: u32 = 32;

/// Level-0 cursor watermark block count. Conservative: a 256-wide ARGB cursor
/// fetches 256x4 = 1024 bytes/line (two 512-byte DBUF blocks), so eight blocks
/// buffers four lines — comfortably above the fetch need and strictly below the
/// cursor's [`CURSOR_DDB_BLOCKS`] allocation (a watermark >= the plane's DDB
/// allocation is invalid). The exact value would be computed from memory
/// latency read over a PCODE mailbox this driver does not implement; this
/// fixed conservative block-based level keeps the primary's fetch un-starved,
/// which is what cures the 8x replication, even if the cursor itself is not
/// power-state-optimal.
pub const CURSOR_WM0_BLOCKS: u32 = 8;

/// A DDB allocation as a half-open block range `[start, end)` (end exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DdbEntry {
    pub start: u32,
    /// One past the last allocated block (exclusive).
    pub end: u32,
}

impl DdbEntry {
    /// Block count in this allocation (0 for an empty/degenerate range).
    pub const fn blocks(self) -> u32 {
        if self.end <= self.start {
            0
        } else {
            self.end - self.start
        }
    }
}

/// The result of carving cursor DDB off the primary's allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorDdbSplit {
    /// The primary's allocation shrunk by the cursor's blocks (its tail removed).
    pub primary: DdbEntry,
    /// The cursor's allocation (the reclaimed tail).
    pub cursor: DdbEntry,
}

/// Decode a `PLANE_BUF_CFG` / `CUR_BUF_CFG` register value into a `[start, end)`
/// block range. The register stores END as the inclusive last block (one less
/// than the exclusive end), so decode adds it back. A zero register — no
/// allocation — decodes to the empty range `[0, 0)`.
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

/// Encode a `[start, end)` block range into a `PLANE_BUF_CFG` / `CUR_BUF_CFG`
/// value, writing END as the inclusive last block (`end - 1`). An empty or
/// degenerate range encodes to 0 (no allocation).
pub const fn encode_buf_cfg(entry: DdbEntry) -> u32 {
    if entry.end <= entry.start {
        return 0;
    }
    regs::reg_field_set(regs::DDB_BUF_START_MASK, entry.start)
        | regs::reg_field_set(regs::DDB_BUF_END_MASK, entry.end - 1)
}

/// Carve `cursor_blocks` DDB blocks off the TAIL of the primary's allocation for
/// the cursor, leaving the primary the remainder: the cursor takes the tail of
/// the pipe allocation and the primary keeps the rest.
///
/// Returns `None` (carve refused, nothing changes) unless the primary keeps
/// strictly MORE blocks than it surrenders, so a degenerate or implausibly small
/// firmware allocation can never produce a starved primary or an inverted range.
pub const fn carve_cursor_ddb(primary: DdbEntry, cursor_blocks: u32) -> Option<CursorDdbSplit> {
    if primary.end <= primary.start {
        return None;
    }
    // Keep > what we surrender: the primary must have more than 2x the cursor's
    // blocks so the shrunk primary still exceeds the cursor allocation.
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

/// Encode a `PLANE_WM` / `CUR_WM` watermark-level value: the enable bit, then
/// either the ignore-lines bit (a purely block-based level) or the lines field,
/// plus the blocks field. A disabled level encodes to 0.
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

/// The level-0 cursor watermark: enabled, block-based (ignore lines), with the
/// conservative [`CURSOR_WM0_BLOCKS`] block count. Higher levels and the
/// transition watermark are left disabled (the display falls back to level 0).
pub const fn cursor_wm0() -> u32 {
    wm_value(true, true, 0, CURSOR_WM0_BLOCKS)
}
