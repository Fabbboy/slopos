//! VirtIO virtqueue implementation
//!
//! Generic split virtqueue that can be reused by all VirtIO drivers.

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{alloc_page_frame, free_page_frame, ALLOC_FLAG_ZERO};

use super::{
    virtio_rmb, virtio_wmb, COMMON_CFG_QUEUE_AVAIL, COMMON_CFG_QUEUE_DESC, COMMON_CFG_QUEUE_ENABLE,
    COMMON_CFG_QUEUE_NOTIFY_OFF, COMMON_CFG_QUEUE_SELECT, COMMON_CFG_QUEUE_SIZE,
    COMMON_CFG_QUEUE_USED, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};

pub const DEFAULT_QUEUE_SIZE: u16 = 64;

/// Performance instrumentation counters for VirtIO queue polling
pub static VIRTIO_FENCE_COUNT: AtomicU64 = AtomicU64::new(0);
pub static VIRTIO_SPIN_COUNT: AtomicU64 = AtomicU64::new(0);
pub static VIRTIO_COMPLETION_COUNT: AtomicU64 = AtomicU64::new(0);

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

    // Avail ring layout: flags (u16) | idx (u16) | ring[size] (u16 each)
    #[allow(dead_code)]
    fn avail_flags_ptr(&self) -> *mut u16 {
        self.avail_virt as *mut u16
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        unsafe { (self.avail_virt as *mut u16).add(1) }
    }

    fn avail_ring_ptr(&self, idx: u16) -> *mut u16 {
        unsafe { (self.avail_virt as *mut u16).add(2 + (idx % self.size) as usize) }
    }

    // Used ring layout: flags (u16) | idx (u16) | ring[size] (VirtqUsedElem each)
    #[allow(dead_code)]
    fn used_flags_ptr(&self) -> *const u16 {
        self.used_virt as *const u16
    }

    fn used_idx_ptr(&self) -> *const u16 {
        unsafe { (self.used_virt as *const u16).add(1) }
    }

    #[allow(dead_code)]
    fn used_ring_ptr(&self, idx: u16) -> *const VirtqUsedElem {
        // Offset: 4 bytes for flags+idx, then ring
        unsafe {
            let base = self.used_virt.add(4);
            (base as *const VirtqUsedElem).add((idx % self.size) as usize)
        }
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
            // This is necessary because volatile alone doesn't guarantee cache coherency
            // on all architectures - we need to invalidate our cache line view.
            // Per VirtIO spec 2.7.13: read barrier before reading used ring.
            virtio_rmb();
            VIRTIO_FENCE_COUNT.fetch_add(1, Ordering::Relaxed);

            let used_idx = self.read_used_idx();
            if used_idx != self.last_used_idx {
                VIRTIO_COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
                self.last_used_idx = used_idx;
                return true;
            }
            spins += 1;
            if spins > timeout_spins {
                VIRTIO_SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
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

pub struct DescriptorChain<'a> {
    queue: &'a Virtqueue,
    head: u16,
    current: u16,
    count: u16,
}

impl<'a> DescriptorChain<'a> {
    pub fn new(queue: &'a Virtqueue, head: u16) -> Self {
        Self {
            queue,
            head,
            current: head,
            count: 0,
        }
    }

    pub fn head(&self) -> u16 {
        self.head
    }

    pub fn add_readable(&mut self, addr: u64, len: u32) -> &mut Self {
        self.add_desc(addr, len, false)
    }

    pub fn add_writable(&mut self, addr: u64, len: u32) -> &mut Self {
        self.add_desc(addr, len, true)
    }

    fn add_desc(&mut self, addr: u64, len: u32, writable: bool) -> &mut Self {
        let idx = self.current;
        let next_idx = idx.wrapping_add(1) % self.queue.size;

        let mut flags = 0u16;
        if writable {
            flags |= VIRTQ_DESC_F_WRITE;
        }
        // Will set NEXT flag in finalize for all but last

        self.queue.write_desc(
            idx,
            VirtqDesc {
                addr,
                len,
                flags,
                next: next_idx,
            },
        );

        self.current = next_idx;
        self.count += 1;
        self
    }

    pub fn finalize(&self) {
        if self.count == 0 {
            return;
        }

        // Go back and set NEXT flags for all descriptors except the last
        for i in 0..self.count {
            let idx = (self.head + i) % self.queue.size;
            if i < self.count - 1 {
                // Read current flags, add NEXT
                unsafe {
                    let desc_ptr = self.queue.desc_virt.add(idx as usize);
                    let mut desc = ptr::read_volatile(desc_ptr);
                    desc.flags |= VIRTQ_DESC_F_NEXT;
                    desc.next = (self.head + i + 1) % self.queue.size;
                    ptr::write_volatile(desc_ptr, desc);
                }
            }
        }
    }
}

pub fn send_command(
    queue: &mut Virtqueue,
    notify_cfg: &MmioRegion,
    notify_off_multiplier: u32,
    queue_index: u16,
    cmd_phys: u64,
    cmd_len: usize,
    resp_phys: u64,
    resp_len: usize,
    timeout_spins: u32,
) -> bool {
    if !queue.is_ready() || !notify_cfg.is_mapped() {
        return false;
    }

    // Descriptor 0: command (device reads)
    queue.write_desc(
        0,
        VirtqDesc {
            addr: cmd_phys,
            len: cmd_len as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: 1,
        },
    );

    // Descriptor 1: response (device writes)
    queue.write_desc(
        1,
        VirtqDesc {
            addr: resp_phys,
            len: resp_len as u32,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0,
        },
    );

    queue.submit(0);
    notify_queue(notify_cfg, notify_off_multiplier, queue, queue_index);
    queue.poll_used(timeout_spins)
}
