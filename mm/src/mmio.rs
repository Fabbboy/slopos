use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::arch::x86_64::paging::PageFlags;
use slopos_lib::align_up_u64;

use crate::mm_constants::{MMIO_VIRT_BASE, MMIO_VIRT_SIZE, PAGE_SIZE_4KB};
use crate::paging::map_page_4kb;

static MMIO_NEXT_VIRT: AtomicU64 = AtomicU64::new(MMIO_VIRT_BASE);

fn mmio_alloc_virt(size: u64) -> Option<u64> {
    let aligned_size = align_up_u64(size, PAGE_SIZE_4KB);
    let mut current = MMIO_NEXT_VIRT.load(Ordering::Relaxed);

    loop {
        let new_next = current.checked_add(aligned_size)?;
        if new_next > MMIO_VIRT_BASE + MMIO_VIRT_SIZE {
            return None;
        }

        match MMIO_NEXT_VIRT.compare_exchange_weak(
            current,
            new_next,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MmioRegion {
    virt_base: u64,
    phys_base: u64,
    size: usize,
}

impl MmioRegion {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            virt_base: 0,
            phys_base: 0,
            size: 0,
        }
    }

    pub fn map(phys: PhysAddr, size: usize) -> Option<Self> {
        if phys.is_null() || size == 0 {
            return None;
        }

        phys.as_u64().checked_add(size as u64)?;

        let aligned_phys = phys.as_u64() & !(PAGE_SIZE_4KB - 1);
        let offset_in_page = phys.as_u64() - aligned_phys;
        let total_size = align_up_u64(offset_in_page + size as u64, PAGE_SIZE_4KB);
        let num_pages = total_size / PAGE_SIZE_4KB;

        let virt_base = mmio_alloc_virt(total_size)?;

        let mmio_flags = PageFlags::MMIO.bits();

        for i in 0..num_pages {
            let page_phys = PhysAddr::new(aligned_phys + i * PAGE_SIZE_4KB);
            let page_virt = VirtAddr::new(virt_base + i * PAGE_SIZE_4KB);

            if map_page_4kb(page_virt, page_phys, mmio_flags) != 0 {
                return None;
            }
        }

        Some(Self {
            virt_base: virt_base + offset_in_page,
            phys_base: phys.as_u64(),
            size,
        })
    }

    pub fn map_page(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, PAGE_SIZE_4KB as usize)
    }

    pub fn map_1mb(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, 1024 * 1024)
    }

    #[inline]
    pub fn read<T: Copy>(&self, offset: usize) -> T {
        let size = core::mem::size_of::<T>();
        let end = offset.checked_add(size).expect("offset overflow");

        debug_assert!(
            end <= self.size,
            "MMIO read out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );

        debug_assert!(
            offset % size == 0,
            "MMIO read misaligned: offset={}, align={}",
            offset,
            size
        );

        let ptr = (self.virt_base + offset as u64) as *const T;
        unsafe { read_volatile(ptr) }
    }

    #[inline]
    pub fn write<T: Copy>(&self, offset: usize, value: T) {
        let size = core::mem::size_of::<T>();
        let end = offset.checked_add(size).expect("offset overflow");

        debug_assert!(
            end <= self.size,
            "MMIO write out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );

        debug_assert!(
            offset % size == 0,
            "MMIO write misaligned: offset={}, align={}",
            offset,
            size
        );

        let ptr = (self.virt_base + offset as u64) as *mut T;
        unsafe { write_volatile(ptr, value) }
    }

    #[inline]
    pub fn read_u8(&self, offset: usize) -> u8 {
        self.read(offset)
    }

    #[inline]
    pub fn read_u16(&self, offset: usize) -> u16 {
        self.read(offset)
    }

    #[inline]
    pub fn read_u32(&self, offset: usize) -> u32 {
        self.read(offset)
    }

    #[inline]
    pub fn read_u64(&self, offset: usize) -> u64 {
        self.read(offset)
    }

    #[inline]
    pub fn write_u8(&self, offset: usize, value: u8) {
        self.write(offset, value)
    }

    #[inline]
    pub fn write_u16(&self, offset: usize, value: u16) {
        self.write(offset, value)
    }

    #[inline]
    pub fn write_u32(&self, offset: usize, value: u32) {
        self.write(offset, value)
    }

    #[inline]
    pub fn write_u64(&self, offset: usize, value: u64) {
        self.write(offset, value)
    }

    #[inline]
    pub fn virt_base(&self) -> u64 {
        self.virt_base
    }

    #[inline]
    pub fn phys_base(&self) -> PhysAddr {
        PhysAddr::new(self.phys_base)
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn is_mapped(&self) -> bool {
        self.size != 0
    }

    #[inline]
    pub fn is_valid_offset(&self, offset: usize, access_size: usize) -> bool {
        offset
            .checked_add(access_size)
            .is_some_and(|end| end <= self.size)
    }

    pub fn sub_region(&self, offset: usize, size: usize) -> Option<MmioRegion> {
        let end = offset.checked_add(size)?;
        if end > self.size {
            return None;
        }
        Some(MmioRegion {
            virt_base: self.virt_base + offset as u64,
            phys_base: self.phys_base + offset as u64,
            size,
        })
    }
}

impl Default for MmioRegion {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

unsafe impl Send for MmioRegion {}
