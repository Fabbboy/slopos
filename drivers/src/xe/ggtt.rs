use slopos_abi::PhysAddr;
use slopos_lib::align_up_u64;
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::mmio::MmioRegion;

use super::regs;

#[derive(Copy, Clone)]
pub struct XeGgtt {
    pub table: MmioRegion,
    pub entries: u32,
    pub next_entry: u32,
}

impl XeGgtt {
    pub const fn empty() -> Self {
        Self {
            table: MmioRegion::empty(),
            entries: 0,
            next_entry: regs::GGTT_START_ENTRY,
        }
    }
}

pub fn xe_ggtt_init(mmio: &MmioRegion) -> Option<XeGgtt> {
    if !mmio.is_mapped() {
        return None;
    }

    let table_size = ggtt_table_size(mmio.size())?;
    let table = mmio.sub_region(regs::GTTMMADR_GGTT_OFFSET, table_size)?;
    let entries = (table.size() / regs::GGTT_PTE_BYTES) as u32;
    if entries <= regs::GGTT_START_ENTRY {
        return None;
    }

    Some(XeGgtt {
        table,
        entries,
        next_entry: regs::GGTT_START_ENTRY,
    })
}

pub fn xe_ggtt_alloc(ggtt: &mut XeGgtt, pages: u32, align_pages: u32) -> Option<u32> {
    if pages == 0 || ggtt.entries == 0 {
        return None;
    }

    let align = align_pages.max(1) as u64;
    let aligned = align_up_u64(ggtt.next_entry as u64, align) as u32;
    let end = aligned.checked_add(pages)?;
    if end >= ggtt.entries {
        return None;
    }

    ggtt.next_entry = end;
    Some(aligned)
}

pub fn xe_ggtt_map(ggtt: &XeGgtt, start_entry: u32, phys: PhysAddr, pages: u32) -> bool {
    if pages == 0 || ggtt.entries == 0 {
        return false;
    }

    let end = match start_entry.checked_add(pages) {
        Some(val) => val,
        None => return false,
    };
    if end > ggtt.entries {
        return false;
    }

    for page in 0..pages {
        let addr = phys.offset(page as u64 * PAGE_SIZE_4KB).as_u64();
        let pte = (addr & regs::GGTT_PTE_ADDR_MASK) | regs::GGTT_PTE_PRESENT;
        let offset = (start_entry + page) as usize * regs::GGTT_PTE_BYTES;
        ggtt.table.write_u64(offset, pte);
    }

    let last_offset = (end - 1) as usize * regs::GGTT_PTE_BYTES;
    let _ = ggtt.table.read_u64(last_offset);
    true
}

fn ggtt_table_size(mmio_size: usize) -> Option<usize> {
    if mmio_size <= regs::GTTMMADR_GGTT_OFFSET {
        return None;
    }

    let available = mmio_size - regs::GTTMMADR_GGTT_OFFSET;
    if available >= regs::GGTT_TABLE_SIZE_8MB {
        Some(regs::GGTT_TABLE_SIZE_8MB)
    } else if available >= regs::GGTT_TABLE_SIZE_4MB {
        Some(regs::GGTT_TABLE_SIZE_4MB)
    } else {
        None
    }
}
