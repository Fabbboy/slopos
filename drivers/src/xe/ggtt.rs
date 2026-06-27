//! GGTT page-table programming.
//!
//! Writes its own GGTT PTEs ONLY at entries strictly above the firmware
//! framebuffer extent (the placement is chosen by the pure
//! [`crate::xe_logic::ggtt_pte::alloc_above`]). It never rewrites firmware PTEs
//! and never zeroes the GGTT. The `bank` handle is the GGTT page-table
//! sub-region carved by [`super::mmio_map::ggtt_bank`]; byte offset `i * 8` is
//! the PTE for entry `i`.

use slopos_abi::PhysAddr;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;

use crate::xe_logic::{ggtt_pte, regs};

/// Map `pages` contiguous 4 KiB physical pages of `phys` into the GGTT starting
/// at GPU virtual byte address `ggtt_addr`. Returns `true` once every PTE is
/// written, `false` if any page address fails to encode or any entry would fall
/// outside the page-table bank.
///
/// Entry `entry_index(ggtt_addr) + p` maps physical page `p`, and its PTE lives
/// at bank byte offset `entry * GGTT_PTE_BYTES`. The caller guarantees
/// `ggtt_addr` (and therefore every entry written here) sits strictly above the
/// firmware framebuffer extent — chosen via
/// [`crate::xe_logic::ggtt_pte::alloc_above`] — so this loop never touches a
/// firmware PTE. No firmware PTE is read, and nothing is zeroed. Every
/// arithmetic step is checked, and each write is bounds-checked against
/// [`MmioRegion::size`] before it is issued, so a malformed request stops
/// (writing nothing further) rather than running off the bank.
pub fn map_pages(bank: &MmioRegion, ggtt_addr: u64, phys: PhysAddr, pages: u32) -> bool {
    let base_entry = ggtt_pte::entry_index(ggtt_addr);

    for p in 0..pages {
        // Physical address of page `p`, with checked arithmetic at every step.
        let page_byte_offset = match (p as u64).checked_mul(PAGE_SIZE_4KB) {
            Some(offset) => offset,
            None => return false,
        };
        let page_phys = match phys.checked_offset(page_byte_offset) {
            Some(addr) => addr,
            None => return false,
        };
        let pte = match ggtt_pte::pte_encode(page_phys.as_u64()) {
            Some(pte) => pte,
            None => return false,
        };

        // Bank byte offset of the PTE for entry `base_entry + p`.
        let entry = match base_entry.checked_add(p) {
            Some(entry) => entry,
            None => return false,
        };
        let pte_offset = match (entry as usize).checked_mul(regs::GGTT_PTE_BYTES) {
            Some(offset) => offset,
            None => return false,
        };

        // Refuse to write past the page-table bank; the full u64 PTE must fit.
        if !bank.is_valid_offset(pte_offset, regs::GGTT_PTE_BYTES) {
            return false;
        }

        bank.write::<u64>(pte_offset, pte);
    }

    // Post the PTE writes: read the last entry back so the MMIO store queue is
    // drained into the GGTT before the display engine is told to re-read it.
    let last_entry = base_entry + (pages - 1);
    let last_offset = (last_entry as usize) * regs::GGTT_PTE_BYTES;
    if bank.is_valid_offset(last_offset, regs::GGTT_PTE_BYTES) {
        let _ = bank.read::<u64>(last_offset);
    }

    true
}

/// Invalidate the display engine's GGTT TLB so it observes freshly written
/// PTEs. The firmware's scanout runs off a TLB-warm GGTT, but PTEs written here
/// via MMIO are invisible to the engine until the TLB is flushed; without this
/// the plane scans stale translations. `mmio` is the full BAR0 register window
/// (the flush register lives in its low half, not in the GGTT bank). Followed by
/// a posting read so the flush lands before scanout is re-pointed.
pub fn invalidate_tlb(mmio: &MmioRegion) {
    mmio.write::<u32>(regs::GFX_FLSH_CNTL_GEN6, regs::GFX_FLSH_CNTL_EN);
    let _ = mmio.read::<u32>(regs::GFX_FLSH_CNTL_GEN6);
}
