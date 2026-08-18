//! Global GTT page-table-entry encoding and framebuffer-placement arithmetic.
//!
//! Pure bit math over plain integers: no MMIO, no allocation, no I/O. Every
//! arithmetic step is checked or saturating, so a malformed input yields
//! `None`/`false` rather than a panic.

use super::regs;

/// Address-space granularity, distinct from a PTE's on-table width
/// (`regs::GGTT_PTE_BYTES`): entry *N* maps the page at GPU VA byte `N * 4096`.
pub const PAGE_SIZE_BYTES: u64 = 4096;

/// `None` when `phys` is not 4 KiB-aligned. Bits outside the address field are
/// masked away, mirroring the silicon reading those positions back as zero.
pub fn pte_encode(phys: u64) -> Option<u64> {
    if phys & (PAGE_SIZE_BYTES - 1) != 0 {
        return None;
    }
    Some((phys & regs::GGTT_PTE_ADDR_MASK) | regs::GGTT_PTE_PRESENT)
}

pub fn ggtt_byte_offset(entry_index: u32) -> u64 {
    (entry_index as u64) * PAGE_SIZE_BYTES
}

/// Page-division floor: a sub-page remainder is dropped, so this inverts
/// `ggtt_byte_offset` only for page-aligned inputs.
pub fn entry_index(ggtt_byte_offset: u64) -> u32 {
    (ggtt_byte_offset / PAGE_SIZE_BYTES) as u32
}

/// Ranges are half-open, so a zero-length one never overlaps. End points
/// saturate so an oversized `start + len` cannot wrap.
pub fn region_overlaps(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    if a_len == 0 || b_len == 0 {
        return false;
    }
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

/// `None` on a zero alignment or on overflow of the rounding addition.
fn align_up(value: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let bumped = value.checked_add(align - 1)?;
    Some(bumped - (bumped % align))
}

/// A GGTT byte offset for `pages` contiguous 4 KiB pages, `align`-aligned and
/// above the firmware framebuffer extent `[fw_surf, fw_surf + fw_len)`. Rounding
/// that extent's exclusive end up to `align` puts the start at or beyond
/// `fw_surf + fw_len`, so the result never collides with the firmware surface.
/// `None` on zero alignment, on any overflow, or when the request does not fit
/// inside `ggtt_total_bytes`.
pub fn alloc_above(
    fw_surf: u64,
    fw_len: u64,
    align: u64,
    pages: u32,
    ggtt_total_bytes: u64,
) -> Option<u64> {
    let fw_end = fw_surf.checked_add(fw_len)?;
    let size = (pages as u64).checked_mul(PAGE_SIZE_BYTES)?;
    let start = align_up(fw_end, align)?;
    let end = start.checked_add(size)?;
    if end <= ggtt_total_bytes {
        Some(start)
    } else {
        None
    }
}
