//! GGTT page-table programming. Writes PTEs ONLY at entries strictly above the
//! firmware framebuffer extent, never rewriting a firmware PTE and never zeroing
//! the GGTT. The `bank` handle is the page-table sub-region carved by
//! [`super::mmio_map::ggtt_bank`]; byte offset `i * 8` is the PTE for entry `i`.

use slopos_abi::PhysAddr;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;

use crate::xe_logic::{ggtt_pte, regs};

/// Map `pages` contiguous 4 KiB physical pages of `phys` into the GGTT starting
/// at GPU virtual byte address `ggtt_addr`. `false` if any page address fails to
/// encode or any entry would fall outside the page-table bank, having written
/// only the entries preceding the failure.
///
/// The caller must supply a `ggtt_addr` strictly above the firmware framebuffer
/// extent — see [`crate::xe_logic::ggtt_pte::alloc_above`] — so no firmware PTE
/// is ever written.
pub fn map_pages(bank: &MmioRegion, ggtt_addr: u64, phys: PhysAddr, pages: u32) -> bool {
    let base_entry = ggtt_pte::entry_index(ggtt_addr);

    for p in 0..pages {
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

        let entry = match base_entry.checked_add(p) {
            Some(entry) => entry,
            None => return false,
        };
        let pte_offset = match (entry as usize).checked_mul(regs::GGTT_PTE_BYTES) {
            Some(offset) => offset,
            None => return false,
        };

        if !bank.is_valid_offset(pte_offset, regs::GGTT_PTE_BYTES) {
            return false;
        }

        bank.write::<u64>(pte_offset, pte);
    }

    // Posting read: drains the MMIO store queue into the GGTT before the display
    // engine is told to re-read it.
    let last_entry = base_entry + (pages - 1);
    let last_offset = (last_entry as usize) * regs::GGTT_PTE_BYTES;
    if bank.is_valid_offset(last_offset, regs::GGTT_PTE_BYTES) {
        let _ = bank.read::<u64>(last_offset);
    }

    true
}

/// Invalidate the display engine's GGTT TLB: PTEs written via MMIO stay invisible
/// to the engine until it is flushed, so without this the plane scans stale
/// translations. `mmio` is the full BAR0 window — the flush register lives in its
/// low half, not in the GGTT bank. The posting read makes the flush land before
/// scanout is re-pointed.
pub fn invalidate_tlb(mmio: &MmioRegion) {
    mmio.write::<u32>(regs::GFX_FLSH_CNTL_GEN6, regs::GFX_FLSH_CNTL_EN);
    let _ = mmio.read::<u32>(regs::GFX_FLSH_CNTL_GEN6);
}
