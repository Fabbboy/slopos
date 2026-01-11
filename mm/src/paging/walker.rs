use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::arch::x86_64::page_table::{PageTable, PageTableEntry, PageTableLevel};
use slopos_abi::arch::x86_64::paging::PAGE_SIZE_4KB;

use super::error::{PagingError, PagingResult};
use crate::hhdm::PhysAddrHhdm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkAction {
    Descend,
    Stop,
    Skip,
}

impl Default for WalkAction {
    fn default() -> Self {
        Self::Descend
    }
}

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

pub unsafe trait PageTableFrameMapping {
    fn phys_to_table_ptr(&self, phys: PhysAddr) -> Option<*mut PageTable>;
}

pub struct HhdmMapping;

unsafe impl PageTableFrameMapping for HhdmMapping {
    #[inline]
    fn phys_to_table_ptr(&self, phys: PhysAddr) -> Option<*mut PageTable> {
        if phys.is_null() {
            return None;
        }
        Some(phys.to_virt().as_mut_ptr())
    }
}

pub struct PageTableWalker<M: PageTableFrameMapping = HhdmMapping> {
    mapping: M,
}

impl PageTableWalker<HhdmMapping> {
    #[inline]
    pub fn new() -> Self {
        Self {
            mapping: HhdmMapping,
        }
    }
}

impl Default for PageTableWalker<HhdmMapping> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: PageTableFrameMapping> PageTableWalker<M> {
    #[inline]
    pub fn with_mapping(mapping: M) -> Self {
        Self { mapping }
    }

    #[inline]
    pub fn next_table<'a>(
        &self,
        entry: &PageTableEntry,
        level: PageTableLevel,
    ) -> PagingResult<&'a PageTable> {
        if !entry.is_present() {
            return Err(PagingError::NotMapped {
                address: entry.address().as_u64(),
                level,
            });
        }
        if entry.is_huge() && level.supports_huge_pages() {
            return Err(PagingError::MappedToHugePage { level });
        }

        let phys = entry.address();
        let ptr = self
            .mapping
            .phys_to_table_ptr(phys)
            .ok_or(PagingError::InvalidPageTable)?;

        Ok(unsafe { &*ptr })
    }

    #[inline]
    pub fn next_table_mut<'a>(
        &self,
        entry: &PageTableEntry,
        level: PageTableLevel,
    ) -> PagingResult<&'a mut PageTable> {
        if !entry.is_present() {
            return Err(PagingError::NotMapped {
                address: entry.address().as_u64(),
                level,
            });
        }
        if entry.is_huge() && level.supports_huge_pages() {
            return Err(PagingError::MappedToHugePage { level });
        }

        let phys = entry.address();
        let ptr = self
            .mapping
            .phys_to_table_ptr(phys)
            .ok_or(PagingError::InvalidPageTable)?;

        Ok(unsafe { &mut *ptr })
    }

    pub fn walk(&self, pml4: &PageTable, vaddr: VirtAddr) -> PagingResult<WalkResult> {
        let mut current_table = pml4;
        let mut level = PageTableLevel::Four;

        loop {
            let index = level.index_of(vaddr);
            let entry = current_table[index];

            if !entry.is_present() {
                return Err(PagingError::NotMapped {
                    address: vaddr.as_u64(),
                    level,
                });
            }

            if entry.is_huge() && level.supports_huge_pages() {
                let page_size = level.page_size().unwrap();
                let offset = vaddr.as_u64() & (page_size - 1);
                return Ok(WalkResult {
                    entry,
                    level,
                    phys_addr: entry.address().offset(offset),
                    page_size,
                });
            }

            match level.next_lower() {
                Some(next_level) => {
                    current_table = self.next_table(&entry, level)?;
                    level = next_level;
                }
                None => {
                    let offset = vaddr.as_u64() & (PAGE_SIZE_4KB - 1);
                    return Ok(WalkResult {
                        entry,
                        level,
                        phys_addr: entry.address().offset(offset),
                        page_size: PAGE_SIZE_4KB,
                    });
                }
            }
        }
    }

    pub fn walk_with<F>(
        &self,
        pml4: &PageTable,
        vaddr: VirtAddr,
        mut callback: F,
    ) -> PagingResult<WalkResult>
    where
        F: FnMut(PageTableLevel, &PageTableEntry) -> WalkAction,
    {
        let mut current_table = pml4;
        let mut level = PageTableLevel::Four;

        loop {
            let index = level.index_of(vaddr);
            let entry = &current_table[index];

            match callback(level, entry) {
                WalkAction::Stop => {
                    let page_size = level.page_size().unwrap_or(PAGE_SIZE_4KB);
                    let offset = vaddr.as_u64() & (page_size - 1);
                    return Ok(WalkResult {
                        entry: *entry,
                        level,
                        phys_addr: entry.address().offset(offset),
                        page_size,
                    });
                }
                WalkAction::Skip => {
                    return Err(PagingError::NotMapped {
                        address: vaddr.as_u64(),
                        level,
                    });
                }
                WalkAction::Descend => {
                    if !entry.is_present() {
                        return Err(PagingError::NotMapped {
                            address: vaddr.as_u64(),
                            level,
                        });
                    }

                    if entry.is_huge() && level.supports_huge_pages() {
                        let page_size = level.page_size().unwrap();
                        let offset = vaddr.as_u64() & (page_size - 1);
                        return Ok(WalkResult {
                            entry: *entry,
                            level,
                            phys_addr: entry.address().offset(offset),
                            page_size,
                        });
                    }

                    match level.next_lower() {
                        Some(next_level) => {
                            current_table = self.next_table(entry, level)?;
                            level = next_level;
                        }
                        None => {
                            let offset = vaddr.as_u64() & (PAGE_SIZE_4KB - 1);
                            return Ok(WalkResult {
                                entry: *entry,
                                level,
                                phys_addr: entry.address().offset(offset),
                                page_size: PAGE_SIZE_4KB,
                            });
                        }
                    }
                }
            }
        }
    }

    pub fn walk_levels(
        &self,
        pml4: &PageTable,
        vaddr: VirtAddr,
    ) -> (
        Option<PageTableEntry>,
        Option<PageTableEntry>,
        Option<PageTableEntry>,
        Option<PageTableEntry>,
    ) {
        let l4_idx = PageTableLevel::Four.index_of(vaddr);
        let l4_entry = pml4[l4_idx];

        if !l4_entry.is_present() {
            return (Some(l4_entry), None, None, None);
        }

        let l3 = match self.next_table(&l4_entry, PageTableLevel::Four) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), None, None, None),
        };

        let l3_idx = PageTableLevel::Three.index_of(vaddr);
        let l3_entry = l3[l3_idx];

        if !l3_entry.is_present() || l3_entry.is_huge() {
            return (Some(l4_entry), Some(l3_entry), None, None);
        }

        let l2 = match self.next_table(&l3_entry, PageTableLevel::Three) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), Some(l3_entry), None, None),
        };

        let l2_idx = PageTableLevel::Two.index_of(vaddr);
        let l2_entry = l2[l2_idx];

        if !l2_entry.is_present() || l2_entry.is_huge() {
            return (Some(l4_entry), Some(l3_entry), Some(l2_entry), None);
        }

        let l1 = match self.next_table(&l2_entry, PageTableLevel::Two) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), Some(l3_entry), Some(l2_entry), None),
        };

        let l1_idx = PageTableLevel::One.index_of(vaddr);
        let l1_entry = l1[l1_idx];

        (
            Some(l4_entry),
            Some(l3_entry),
            Some(l2_entry),
            Some(l1_entry),
        )
    }
}
