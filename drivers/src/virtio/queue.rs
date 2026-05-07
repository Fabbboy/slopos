use core::mem::size_of;

use slopos_abi::addr::PhysAddr;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_ostd::Pod;
use slopos_ostd::dma::VirtqueueRegion;
use slopos_ostd::mm::frame::{Frame, KernelMeta};

use super::{
    COMMON_CFG_QUEUE_AVAIL, COMMON_CFG_QUEUE_DESC, COMMON_CFG_QUEUE_ENABLE,
    COMMON_CFG_QUEUE_MSIX_VECTOR, COMMON_CFG_QUEUE_NOTIFY_OFF, COMMON_CFG_QUEUE_SELECT,
    COMMON_CFG_QUEUE_SIZE, COMMON_CFG_QUEUE_USED, VIRTIO_MSI_NO_VECTOR, virtio_rmb, virtio_wmb,
};

pub const DEFAULT_QUEUE_SIZE: u16 = 64;

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

// Avail ring layout (split virtqueue, packed):
//   u16 flags                       — offset 0
//   u16 idx                         — offset 2
//   u16 ring[size]                  — offset 4
//   u16 used_event (if EVENT_IDX)   — offset 4 + 2*size
const AVAIL_IDX_OFFSET: usize = 2;
const AVAIL_RING_OFFSET: usize = 4;

// Used ring layout (split virtqueue, packed):
//   u16 flags                       — offset 0
//   u16 idx                         — offset 2
//   VirtqUsedElem ring[size]        — offset 4
//   u16 avail_event (if EVENT_IDX)  — offset 4 + 8*size
const USED_IDX_OFFSET: usize = 2;
const USED_RING_OFFSET: usize = 4;

pub struct Virtqueue {
    pub size: u16,
    pub desc_phys: PhysAddr,
    pub avail_phys: PhysAddr,
    pub used_phys: PhysAddr,
    desc_ring: Option<VirtqueueRegion<VirtqDesc>>,
    avail_frame: Option<Frame<KernelMeta>>,
    used_frame: Option<Frame<KernelMeta>>,
    pub notify_off: u16,
    pub last_used_idx: u16,
    pub ready: bool,
}

impl Default for Virtqueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Virtqueue {
    pub const fn new() -> Self {
        Self {
            size: 0,
            desc_phys: PhysAddr::NULL,
            avail_phys: PhysAddr::NULL,
            used_phys: PhysAddr::NULL,
            desc_ring: None,
            avail_frame: None,
            used_frame: None,
            notify_off: 0,
            last_used_idx: 0,
            ready: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn read_used_idx(&self) -> u16 {
        self.used_frame
            .as_ref()
            .and_then(|f| f.read_volatile_at::<u16>(USED_IDX_OFFSET))
            .unwrap_or(0)
    }

    pub fn write_desc(&mut self, idx: u16, desc: VirtqDesc) {
        if idx >= self.size {
            return;
        }
        if let Some(ring) = self.desc_ring.as_mut() {
            ring.write_desc_volatile(idx as usize, desc);
        }
    }

    pub fn submit(&mut self, head: u16) {
        if !self.ready || self.size == 0 {
            return;
        }
        let Some(avail) = self.avail_frame.as_ref() else {
            return;
        };
        let Some(avail_idx) = avail.read_volatile_at::<u16>(AVAIL_IDX_OFFSET) else {
            return;
        };
        let ring_off =
            AVAIL_RING_OFFSET + (avail_idx as usize % self.size as usize) * size_of::<u16>();
        avail.write_volatile_at::<u16>(ring_off, head);
        virtio_wmb();
        avail.write_volatile_at::<u16>(AVAIL_IDX_OFFSET, avail_idx.wrapping_add(1));
    }

    /// Try to pop one entry from the used ring without waiting.
    /// Returns `None` if no new entries are available.
    pub fn try_pop_used(&mut self) -> Option<VirtqUsedElem> {
        if self.size == 0 {
            return None;
        }
        virtio_rmb();
        let used_idx = self.read_used_idx();
        if used_idx == self.last_used_idx {
            return None;
        }

        let used = self.used_frame.as_ref()?;
        let elem_off = USED_RING_OFFSET
            + (self.last_used_idx as usize % self.size as usize) * size_of::<VirtqUsedElem>();
        let elem = used.read_volatile_at::<VirtqUsedElem>(elem_off)?;
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some(elem)
    }

    /// Advance the used ring index if the device posted new entries.
    /// Returns `true` if at least one new entry was consumed.
    pub fn advance_used(&mut self) -> bool {
        virtio_rmb();
        let used_idx = self.read_used_idx();
        if used_idx != self.last_used_idx {
            self.last_used_idx = used_idx;
            true
        } else {
            false
        }
    }
}

/// Set up a virtqueue on the device.
///
/// `msix_vector` is the MSI-X table entry index to assign to this queue.
/// Pass [`VIRTIO_MSI_NO_VECTOR`] (0xFFFF) when MSI-X is not in use.
///
/// Per VirtIO spec §4.1.4.3, the `queue_msix_vector` register is written
/// **before** `queue_enable` so the device sees the vector assignment atomically
/// with queue activation.
pub fn setup_queue(
    common_cfg: &MmioRegion,
    queue_index: u16,
    max_size: u16,
    msix_vector: u16,
) -> Option<Virtqueue> {
    if !common_cfg.is_mapped() {
        return None;
    }

    common_cfg.write::<u16>(COMMON_CFG_QUEUE_SELECT, queue_index);

    let device_max_size = common_cfg.read::<u16>(COMMON_CFG_QUEUE_SIZE);
    if device_max_size == 0 {
        return None;
    }

    let size = device_max_size.min(max_size);
    common_cfg.write::<u16>(COMMON_CFG_QUEUE_SIZE, size);

    let desc_frame = OwnedPageFrame::alloc_zeroed()?;
    let avail_frame = OwnedPageFrame::alloc_zeroed()?;
    let used_frame = OwnedPageFrame::alloc_zeroed()?;

    let desc_phys = PhysAddr::new(desc_frame.phys_u64());
    let avail_phys = PhysAddr::new(avail_frame.phys_u64());
    let used_phys = PhysAddr::new(used_frame.phys_u64());

    let desc_ring = VirtqueueRegion::<VirtqDesc>::new(desc_frame, size as usize)?;

    common_cfg.write::<u64>(COMMON_CFG_QUEUE_DESC, desc_phys.as_u64());
    common_cfg.write::<u64>(COMMON_CFG_QUEUE_AVAIL, avail_phys.as_u64());
    common_cfg.write::<u64>(COMMON_CFG_QUEUE_USED, used_phys.as_u64());

    // Write MSI-X vector BEFORE enabling the queue (VirtIO spec §4.1.4.3.2).
    if msix_vector != VIRTIO_MSI_NO_VECTOR {
        common_cfg.write::<u16>(COMMON_CFG_QUEUE_MSIX_VECTOR, msix_vector);
        let readback = common_cfg.read::<u16>(COMMON_CFG_QUEUE_MSIX_VECTOR);
        if readback == VIRTIO_MSI_NO_VECTOR {
            // Device rejected the vector — drop frames and fail.
            return None;
        }
    }

    common_cfg.write::<u16>(COMMON_CFG_QUEUE_ENABLE, 1);

    let notify_off = common_cfg.read::<u16>(COMMON_CFG_QUEUE_NOTIFY_OFF);

    Some(Virtqueue {
        size,
        desc_phys,
        avail_phys,
        used_phys,
        desc_ring: Some(desc_ring),
        avail_frame: Some(avail_frame),
        used_frame: Some(used_frame),
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
    notify_cfg.write::<u16>(offset as usize, queue_index);
}
