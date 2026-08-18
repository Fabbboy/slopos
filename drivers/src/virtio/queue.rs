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

// Packed split-virtqueue avail ring: u16 flags, u16 idx, u16 ring[size], then
// u16 used_event when EVENT_IDX is negotiated.
const AVAIL_IDX_OFFSET: usize = 2;
const AVAIL_RING_OFFSET: usize = 4;

// Packed split-virtqueue used ring: u16 flags, u16 idx, VirtqUsedElem ring[size],
// then u16 avail_event when EVENT_IDX is negotiated.
const USED_IDX_OFFSET: usize = 2;
const USED_RING_OFFSET: usize = 4;

const DESC_NONE: u16 = u16::MAX;

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
    /// Free chain over descriptor indices, held in kernel memory rather than
    /// threaded through the device-visible `next` fields, so a misbehaving
    /// device cannot corrupt the allocator's bookkeeping.
    free_links: [u16; DEFAULT_QUEUE_SIZE as usize],
    free_head: u16,
    num_free: u16,
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
            free_links: [DESC_NONE; DEFAULT_QUEUE_SIZE as usize],
            free_head: DESC_NONE,
            num_free: 0,
        }
    }

    /// `None` when the ring is exhausted — chains quarantined by a timed-out
    /// request are still owned by the device.
    pub fn alloc_desc(&mut self) -> Option<u16> {
        let head = self.free_head;
        if head == DESC_NONE {
            return None;
        }
        self.free_head = self.free_links[head as usize];
        self.free_links[head as usize] = DESC_NONE;
        self.num_free -= 1;
        Some(head)
    }

    /// Only once the device no longer owns the descriptor: its chain head was
    /// seen on the used ring, or it was never submitted.
    pub fn free_desc(&mut self, idx: u16) {
        if idx >= self.size {
            return;
        }
        self.free_links[idx as usize] = self.free_head;
        self.free_head = idx;
        self.num_free += 1;
    }

    pub fn free_count(&self) -> u16 {
        self.num_free
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

    /// Non-mutating peek: catches an IRQ landing between the last drain and the
    /// `wait` re-park.
    #[inline]
    pub fn has_pending(&self) -> bool {
        if self.size == 0 {
            return false;
        }
        virtio_rmb();
        self.read_used_idx() != self.last_used_idx
    }

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
}

/// Writes driver-side state directly into `out`: a `Virtqueue` is ~200 bytes and
/// must never round-trip through a probe's stack frame. `msix_vector` is an
/// MSI-X table index, or [`VIRTIO_MSI_NO_VECTOR`] when MSI-X is not in use. On
/// failure `out` is left inert and empty.
pub fn setup_queue_into(
    common_cfg: &MmioRegion,
    queue_index: u16,
    max_size: u16,
    msix_vector: u16,
    out: &mut Virtqueue,
) -> bool {
    *out = Virtqueue::new();

    if !common_cfg.is_mapped() {
        return false;
    }

    common_cfg.write::<u16>(COMMON_CFG_QUEUE_SELECT, queue_index);

    let device_max_size = common_cfg.read::<u16>(COMMON_CFG_QUEUE_SIZE);
    if device_max_size == 0 {
        return false;
    }

    let size = device_max_size.min(max_size);
    common_cfg.write::<u16>(COMMON_CFG_QUEUE_SIZE, size);

    // Raw-physical frames, deliberately not `DmaCoherent`: under the boot
    // identity mapper IOVA == phys. Revisit for a real VT-d mapper.
    let Some(desc_frame) = OwnedPageFrame::alloc_zeroed() else {
        return false;
    };
    let Some(avail_frame) = OwnedPageFrame::alloc_zeroed() else {
        return false;
    };
    let Some(used_frame) = OwnedPageFrame::alloc_zeroed() else {
        return false;
    };

    let desc_phys = PhysAddr::new(desc_frame.phys_u64());
    let avail_phys = PhysAddr::new(avail_frame.phys_u64());
    let used_phys = PhysAddr::new(used_frame.phys_u64());

    let Some(desc_ring) = VirtqueueRegion::<VirtqDesc>::new(desc_frame, size as usize) else {
        return false;
    };

    common_cfg.write::<u64>(COMMON_CFG_QUEUE_DESC, desc_phys.as_u64());
    common_cfg.write::<u64>(COMMON_CFG_QUEUE_AVAIL, avail_phys.as_u64());
    common_cfg.write::<u64>(COMMON_CFG_QUEUE_USED, used_phys.as_u64());

    // Write MSI-X vector BEFORE enabling the queue (VirtIO spec §4.1.4.3.2).
    if msix_vector != VIRTIO_MSI_NO_VECTOR {
        common_cfg.write::<u16>(COMMON_CFG_QUEUE_MSIX_VECTOR, msix_vector);
        let readback = common_cfg.read::<u16>(COMMON_CFG_QUEUE_MSIX_VECTOR);
        if readback == VIRTIO_MSI_NO_VECTOR {
            return false;
        }
    }

    common_cfg.write::<u16>(COMMON_CFG_QUEUE_ENABLE, 1);

    out.size = size;
    out.desc_phys = desc_phys;
    out.avail_phys = avail_phys;
    out.used_phys = used_phys;
    out.desc_ring = Some(desc_ring);
    out.avail_frame = Some(avail_frame);
    out.used_frame = Some(used_frame);
    out.notify_off = common_cfg.read::<u16>(COMMON_CFG_QUEUE_NOTIFY_OFF);
    out.last_used_idx = 0;

    for i in 0..size {
        out.free_links[i as usize] = if i + 1 < size { i + 1 } else { DESC_NONE };
    }
    out.free_head = 0;
    out.num_free = size;

    out.ready = true;
    true
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
