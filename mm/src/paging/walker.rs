//! Read-only page-table resolution.
//!
//! The walk takes one entry by value per level through
//! [`super::page_table_defs::entry_at`], so it never forms a reference
//! into a page-table frame and needs no receiver to anchor one to.

use super::page_table_defs::{PageTableEntry, PageTableLevel, entry_at};
use crate::paging_defs::PAGE_SIZE_4KB;
use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::error::{MmError, MmResult};

#[derive(Debug, Clone, Copy)]
pub struct WalkResult {
    pub entry: PageTableEntry,
    pub level: PageTableLevel,
    pub phys_addr: PhysAddr,
    pub page_size: u64,
}

impl WalkResult {
    #[inline]
    pub fn is_huge_page(&self) -> bool {
        self.page_size > PAGE_SIZE_4KB
    }
}

/// Resolve `vaddr` under the PML4 frame at `pml4_phys`, stopping at the
/// first huge leaf or at the 4 KiB leaf.
pub fn walk_phys(pml4_phys: PhysAddr, vaddr: VirtAddr) -> MmResult<WalkResult> {
    let mut table_phys = pml4_phys;
    let mut level = PageTableLevel::Four;

    loop {
        if table_phys.is_null() {
            return Err(MmError::InvalidPageTable);
        }

        let entry = entry_at(table_phys, level.index_of(vaddr));
        if !entry.is_present() {
            return Err(MmError::NotMapped {
                address: vaddr.as_u64(),
                level,
            });
        }

        if entry.is_huge() && level.supports_huge_pages() {
            // `supports_huge_pages` is exactly the set of levels with a
            // page size, so the fallback is unreachable — it is here so a
            // read-only walk in a `#![forbid(unsafe_code)]` crate carries
            // no panic edge.
            let page_size = level.page_size().unwrap_or(PAGE_SIZE_4KB);
            return Ok(WalkResult {
                entry,
                level,
                phys_addr: entry.address().offset(vaddr.as_u64() & (page_size - 1)),
                page_size,
            });
        }

        match level.next_lower() {
            Some(next) => {
                table_phys = entry.address();
                level = next;
            }
            None => {
                return Ok(WalkResult {
                    entry,
                    level,
                    phys_addr: entry.address().offset(vaddr.as_u64() & (PAGE_SIZE_4KB - 1)),
                    page_size: PAGE_SIZE_4KB,
                });
            }
        }
    }
}
