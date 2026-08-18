use core::ptr::NonNull;

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

/// Entries in a page-table frame: one 4 KiB page of 8-byte entries.
pub const PAGE_TABLE_ENTRIES: usize = 512;

// The accessors pun a frame as `*mut u64`, so a `PageTableEntry` must stay
// exactly a `u64` in size and alignment.
const _: () = assert!(
    core::mem::size_of::<PageTableEntry>() == core::mem::size_of::<u64>()
        && core::mem::align_of::<PageTableEntry>() == core::mem::align_of::<u64>(),
    "a page-table entry is exactly a u64"
);
const _: () = assert!(
    PAGE_TABLE_ENTRIES * core::mem::size_of::<PageTableEntry>() == PAGE_SIZE_4KB as usize,
    "a page-table frame is one 4 KiB page of entries"
);

// Read side of the page-table descent; every kernel-half write goes through
// `slopos_ostd::mm::vm_space::CursorMut` under the `KERNEL_VM_SPACE` lock.
// Access is per-entry and atomic, never a reference over the frame: the
// hardware walker stamps Accessed and Dirty into entries concurrently.

/// The HHDM view of the page-table frame at `phys`, as an entry array.
#[inline]
fn table_base_at(phys: PhysAddr) -> NonNull<u64> {
    debug_assert!(!phys.is_null(), "page-table frame address must be non-null");
    debug_assert!(
        phys.as_u64() % PAGE_SIZE_4KB == 0,
        "page-table frame address must be page-aligned"
    );
    NonNull::new(phys.to_virt().as_mut_ptr::<u64>())
        .expect("page-table frame address must be non-null")
}

#[inline]
pub(crate) fn entry_at(phys: PhysAddr, index: usize) -> PageTableEntry {
    debug_assert!(index < PAGE_TABLE_ENTRIES);
    slopos_ostd::util::ptr_buf::with_atomic_u64_in_page(table_base_at(phys), index, |slot| {
        PageTableEntry::from_raw(slot.load(core::sync::atomic::Ordering::Relaxed))
    })
}
