use core::ptr;

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};

use super::{
    COMMON_CFG_QUEUE_AVAIL, COMMON_CFG_QUEUE_DESC, COMMON_CFG_QUEUE_ENABLE,
    COMMON_CFG_QUEUE_NOTIFY_OFF, COMMON_CFG_QUEUE_SELECT, COMMON_CFG_QUEUE_SIZE,
    COMMON_CFG_QUEUE_USED, virtio_rmb, virtio_wmb,
};

pub const DEFAULT_QUEUE_SIZE: u16 = 64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

// VirtqAvail and VirtqUsed have variable-size ring arrays.
// We define accessor functions instead of fixed-size structs.

#[repr(C)]
pub struct Virtqueue {
    pub size: u16,
    pub desc_phys: PhysAddr,
    pub avail_phys: PhysAddr,
    pub used_phys: PhysAddr,
    desc_virt: *mut VirtqDesc,
    avail_virt: *mut u8,
    used_virt: *mut u8,
    pub notify_off: u16,
    pub last_used_idx: u16,
    pub ready: bool,
}

impl Default for Virtqueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Virtqueue {
    fn clone(&self) -> Self {
        Self {
            size: self.size,
            desc_phys: self.desc_phys,
            avail_phys: self.avail_phys,
            used_phys: self.used_phys,
            desc_virt: self.desc_virt,
            avail_virt: self.avail_virt,
            used_virt: self.used_virt,
            notify_off: self.notify_off,
            last_used_idx: self.last_used_idx,
            ready: self.ready,
        }
    }
}

impl Copy for Virtqueue {}

// Virtqueue contains raw pointers to page-frame-allocated memory that persists
// for the device lifetime. The memory is kernel-owned and accessible from any
// CPU context, making it safe to transfer ownership between threads.
unsafe impl Send for Virtqueue {}

impl Virtqueue {
    pub const fn new() -> Self {
        Self {
            size: 0,
            desc_phys: PhysAddr::NULL,
            avail_phys: PhysAddr::NULL,
            used_phys: PhysAddr::NULL,
            desc_virt: ptr::null_mut(),
            avail_virt: ptr::null_mut(),
            used_virt: ptr::null_mut(),
            notify_off: 0,
            last_used_idx: 0,
            ready: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        unsafe { (self.avail_virt as *mut u16).add(1) }
    }

    fn avail_ring_ptr(&self, idx: u16) -> *mut u16 {
        unsafe { (self.avail_virt as *mut u16).add(2 + (idx % self.size) as usize) }
    }

    fn used_idx_ptr(&self) -> *const u16 {
        unsafe { (self.used_virt as *const u16).add(1) }
    }

    pub fn read_used_idx(&self) -> u16 {
        unsafe { ptr::read_volatile(self.used_idx_ptr()) }
    }

    pub fn write_desc(&self, idx: u16, desc: VirtqDesc) {
        if !self.desc_virt.is_null() && idx < self.size {
            unsafe {
                ptr::write_volatile(self.desc_virt.add(idx as usize), desc);
            }
        }
    }

    pub fn submit(&mut self, head: u16) {
        if !self.ready {
            return;
        }

        unsafe {
            let avail_idx = ptr::read_volatile(self.avail_idx_ptr());
            ptr::write_volatile(self.avail_ring_ptr(avail_idx), head);
            virtio_wmb();
            ptr::write_volatile(self.avail_idx_ptr(), avail_idx.wrapping_add(1));
        }
    }

    pub fn poll_used(&mut self, timeout_spins: u32) -> bool {
        let mut spins = 0u32;
        loop {
            // Acquire barrier BEFORE reading used_idx to ensure we see device's write.
            // Per VirtIO spec 2.7.13: read barrier before reading used ring.
            virtio_rmb();

            let used_idx = self.read_used_idx();
            if used_idx != self.last_used_idx {
                self.last_used_idx = used_idx;
                return true;
            }
            spins += 1;
            if spins > timeout_spins {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

pub fn setup_queue(common_cfg: &MmioRegion, queue_index: u16, max_size: u16) -> Option<Virtqueue> {
    if !common_cfg.is_mapped() {
        return None;
    }

    common_cfg.write_u16(COMMON_CFG_QUEUE_SELECT, queue_index);

    let device_max_size = common_cfg.read_u16(COMMON_CFG_QUEUE_SIZE);
    if device_max_size == 0 {
        return None;
    }

    let size = device_max_size.min(max_size);
    common_cfg.write_u16(COMMON_CFG_QUEUE_SIZE, size);

    let desc_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    let avail_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    let used_page = alloc_page_frame(ALLOC_FLAG_ZERO);

    if desc_page.is_null() || avail_page.is_null() || used_page.is_null() {
        if !desc_page.is_null() {
            free_page_frame(desc_page);
        }
        if !avail_page.is_null() {
            free_page_frame(avail_page);
        }
        if !used_page.is_null() {
            free_page_frame(used_page);
        }
        return None;
    }

    let desc_virt = desc_page.to_virt().as_mut_ptr::<VirtqDesc>();
    let avail_virt = avail_page.to_virt().as_mut_ptr::<u8>();
    let used_virt = used_page.to_virt().as_mut_ptr::<u8>();

    common_cfg.write_u64(COMMON_CFG_QUEUE_DESC, desc_page.as_u64());
    common_cfg.write_u64(COMMON_CFG_QUEUE_AVAIL, avail_page.as_u64());
    common_cfg.write_u64(COMMON_CFG_QUEUE_USED, used_page.as_u64());
    common_cfg.write_u16(COMMON_CFG_QUEUE_ENABLE, 1);

    let notify_off = common_cfg.read_u16(COMMON_CFG_QUEUE_NOTIFY_OFF);

    Some(Virtqueue {
        size,
        desc_phys: desc_page,
        avail_phys: avail_page,
        used_phys: used_page,
        desc_virt,
        avail_virt,
        used_virt,
        notify_off,
        last_used_idx: 0,
        ready: true,
    })
}

pub fn notify_queue(
    notify_cfg: &MmioRegion,
    notify_off_multiplier: u32,
    queue: &Virtqueue,
    queue_index: u16,
) {
    let offset = (queue.notify_off as u32) * notify_off_multiplier;
    notify_cfg.write_u16(offset as usize, queue_index);
}
