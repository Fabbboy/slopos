use crate::hhdm::PhysAddrHhdm;
use crate::paging_defs::{PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB, PageFlags};
use slopos_abi::addr::{PhysAddr, VirtAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PageTableLevel {
    Four = 4,
    Three = 3,
    Two = 2,
    One = 1,
}

impl PageTableLevel {
    #[inline]
    pub const fn next_lower(self) -> Option<Self> {
        match self {
            Self::Four => Some(Self::Three),
            Self::Three => Some(Self::Two),
            Self::Two => Some(Self::One),
            Self::One => None,
        }
    }

    #[inline]
    pub const fn next_higher(self) -> Option<Self> {
        match self {
            Self::One => Some(Self::Two),
            Self::Two => Some(Self::Three),
            Self::Three => Some(Self::Four),
            Self::Four => None,
        }
    }

    #[inline]
    pub const fn page_size(self) -> Option<u64> {
        match self {
            Self::Three => Some(PAGE_SIZE_1GB),
            Self::Two => Some(PAGE_SIZE_2MB),
            Self::One => Some(PAGE_SIZE_4KB),
            Self::Four => None,
        }
    }

    #[inline]
    pub const fn supports_huge_pages(self) -> bool {
        matches!(self, Self::Three | Self::Two)
    }

    #[inline]
    pub const fn index_of(self, vaddr: VirtAddr) -> usize {
        let shift = 12 + ((self as u8 - 1) * 9);
        ((vaddr.as_u64() >> shift) & 0x1FF) as usize
    }

    #[inline]
    pub const fn entry_size(self) -> u64 {
        1u64 << (12 + ((self as u8 - 1) * 9))
    }

    #[inline]
    pub const fn align_mask(self) -> u64 {
        !(self.entry_size() - 1)
    }

    #[inline]
    pub const fn offset_mask(self) -> u64 {
        self.entry_size() - 1
    }

    #[inline]
    pub const fn is_aligned(self, vaddr: VirtAddr) -> bool {
        vaddr.as_u64() & self.offset_mask() == 0
    }

    #[inline]
    pub const fn align_down(self, vaddr: VirtAddr) -> VirtAddr {
        VirtAddr(vaddr.as_u64() & self.align_mask())
    }
}

impl core::fmt::Display for PageTableLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Four => write!(f, "PML4"),
            Self::Three => write!(f, "PDPT"),
            Self::Two => write!(f, "PD"),
            Self::One => write!(f, "PT"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const EMPTY: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn new(addr: PhysAddr, flags: PageFlags) -> Self {
        Self((addr.as_u64() & PageFlags::ADDRESS_MASK) | flags.bits())
    }

    #[inline]
    pub const fn is_present(&self) -> bool {
        self.0 & PageFlags::PRESENT.bits() != 0
    }

    #[inline]
    pub const fn is_huge(&self) -> bool {
        self.0 & PageFlags::HUGE.bits() != 0
    }

    #[inline]
    pub const fn is_user(&self) -> bool {
        self.0 & PageFlags::USER.bits() != 0
    }

    #[inline]
    pub const fn is_writable(&self) -> bool {
        self.0 & PageFlags::WRITABLE.bits() != 0
    }

    #[inline]
    pub const fn is_unused(&self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn address(&self) -> PhysAddr {
        PhysAddr(self.0 & PageFlags::ADDRESS_MASK)
    }

    #[inline]
    pub const fn flags(&self) -> PageFlags {
        PageFlags::from_bits_truncate(self.0)
    }

    #[inline]
    pub fn set(&mut self, addr: PhysAddr, flags: PageFlags) {
        self.0 = (addr.as_u64() & PageFlags::ADDRESS_MASK) | flags.bits();
    }

    #[inline]
    pub fn set_flags(&mut self, flags: PageFlags) {
        self.0 = (self.0 & PageFlags::ADDRESS_MASK) | flags.bits();
    }

    #[inline]
    pub fn add_flags(&mut self, flags: PageFlags) {
        self.0 |= flags.bits();
    }

    #[inline]
    pub fn remove_flags(&mut self, flags: PageFlags) {
        self.0 &= !flags.bits();
    }

    #[inline]
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    #[inline]
    pub const fn points_to_table(&self) -> bool {
        self.is_present() && !self.is_huge()
    }

    /// If this entry points to a subtable (present, non-huge), return the
    /// virtual pointer to that table via the HHDM. Returns null if the entry
    /// is not present, is a huge-page mapping, or the address is null.
    #[inline]
    pub fn table_ptr(&self) -> *mut PageTable {
        if !self.points_to_table() {
            return core::ptr::null_mut();
        }
        self.address().to_virt().as_mut_ptr()
    }
}

impl Default for PageTableEntry {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PTE({:#x})", self.0)
    }
}

pub const PAGE_TABLE_ENTRIES: usize = 512;

/// A 512-entry page table, aligned to 4KB.
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRIES],
}

impl PageTable {
    pub const EMPTY: Self = Self {
        entries: [PageTableEntry::EMPTY; PAGE_TABLE_ENTRIES],
    };

    #[inline]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    #[inline]
    pub fn entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }

    #[inline]
    pub fn entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }

    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| e.is_unused())
    }

    pub fn zero(&mut self) {
        self.entries.fill(PageTableEntry::EMPTY);
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &PageTableEntry> {
        self.entries.iter()
    }
}

impl core::ops::Index<usize> for PageTable {
    type Output = PageTableEntry;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl core::ops::IndexMut<usize> for PageTable {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.entries[index]
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::EMPTY
    }
}
