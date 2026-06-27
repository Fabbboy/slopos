//! Global GTT page-table-entry encoding and framebuffer-placement arithmetic.
//!
//! Pure bit math over plain integers: no MMIO, no allocation, no I/O. The
//! hardware-sequencing half supplies the BAR0 base and the firmware-decoded
//! framebuffer extent; these functions decide *where* in the GGTT a new mapping
//! lands and *what* each page-table entry must contain. Every arithmetic step is
//! checked or saturating, so a malformed input yields `None`/`false` rather than
//! a panic.

use super::regs;

/// Bytes of GPU virtual address space covered by one GGTT entry (a 4 KiB page).
/// This is the address-space granularity, distinct from a PTE's on-table width
/// (`regs::GGTT_PTE_BYTES`): entry *N* maps the page at GPU VA byte `N * 4096`.
pub const PAGE_SIZE_BYTES: u64 = 4096;

/// Encode the Gen12 GGTT entry that maps physical page `phys`.
///
/// Returns the entry with the present bit set and the page-frame address folded
/// into the address field, or `None` when `phys` is not 4 KiB-aligned. Bits
/// outside the address field are masked away, mirroring the silicon reading
/// those positions back as zero.
pub fn pte_encode(phys: u64) -> Option<u64> {
    if phys & (PAGE_SIZE_BYTES - 1) != 0 {
        return None;
    }
    Some((phys & regs::GGTT_PTE_ADDR_MASK) | regs::GGTT_PTE_PRESENT)
}

/// GPU virtual-address byte offset of the page mapped by `entry_index`.
pub fn ggtt_byte_offset(entry_index: u32) -> u64 {
    (entry_index as u64) * PAGE_SIZE_BYTES
}

/// Entry index covering `ggtt_byte_offset` (the page-division floor; a
/// sub-page remainder is dropped, making this the inverse of
/// `ggtt_byte_offset` for page-aligned inputs).
pub fn entry_index(ggtt_byte_offset: u64) -> u32 {
    (ggtt_byte_offset / PAGE_SIZE_BYTES) as u32
}

/// Whether the half-open byte ranges `[a_start, a_start + a_len)` and
/// `[b_start, b_start + b_len)` share any byte. A zero-length range covers no
/// bytes and therefore never overlaps. End points saturate so an oversized
/// `start + len` cannot wrap.
pub fn region_overlaps(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    if a_len == 0 || b_len == 0 {
        return false;
    }
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

/// Round `value` up to the next multiple of `align`, returning `None` on a zero
/// alignment or on overflow of the rounding addition.
fn align_up(value: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let bumped = value.checked_add(align - 1)?;
    Some(bumped - (bumped % align))
}

/// Choose a GGTT byte offset for `pages` contiguous 4 KiB pages, `align`-aligned
/// and placed strictly above the firmware framebuffer extent
/// `[fw_surf, fw_surf + fw_len)`, with the whole region fitting inside
/// `ggtt_total_bytes`.
///
/// Rounding the firmware extent's exclusive end up to `align` yields a start at
/// or beyond `fw_surf + fw_len`, so the result never collides with the firmware
/// surface. Returns `None` when the alignment is zero, any sum overflows, or the
/// request cannot fit above the firmware surface within the GGTT.
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
