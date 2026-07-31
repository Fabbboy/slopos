use core::mem::size_of;

use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, BlockDeviceIndex};
use slopos_ostd::KArc;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::wait_queue::WaitOutcome;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, Mutex, SpinLock, WaitQueue};
use slopos_ostd::{klog_debug, klog_info, write_field, write_init_field};

use crate::driver_core::bound::BoundDevice;
use crate::pci::{PciMatch, PciProbeError, ProbeOutcome};
use crate::virtio::{
    self, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VirtioMmioCaps,
    VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok, setup_interrupts,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

use slopos_mm::page_alloc::OwnedPageFrame;

pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
/// Flush the device's volatile write-back cache to non-volatile media. Only
/// valid once `VIRTIO_BLK_F_FLUSH` has been negotiated.
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;

/// `VIRTIO_BLK_F_FLUSH` (device feature bit 9): the device has a write-back
/// cache and honours `VIRTIO_BLK_T_FLUSH` requests. Without this, a write that
/// the device ACKs may still sit in a volatile cache and be lost on power
/// failure, so durability barriers are impossible. We request it as an
/// *optional* feature and degrade to "no device cache to flush" if the backend
/// does not offer it.
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const SECTOR_SIZE: u64 = 512;
const REQUEST_TIMEOUT_MS: u32 = 5000;

/// Maximum sectors per data request — bounded by the single bounce page.
const MAX_DATA_SECTORS: usize = 4096 / SECTOR_SIZE as usize;
/// Bounce-buffer capacity in bytes (one page).
const BOUNCE_CAPACITY: usize = MAX_DATA_SECTORS * SECTOR_SIZE as usize;

/// Concurrent request-slot table depth. Logical requests are serialized
/// by `io_lock`, so at most one slot is `InFlight`; the remaining slots
/// absorb quarantined (timed-out) chains the device still owns.
const NUM_REQUEST_SLOTS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, slopos_ostd::Pod)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
struct VirtioBlkDevice {
    queue: Virtqueue,
    capacity_sectors: u64,
    ready: bool,
    /// `VIRTIO_BLK_F_FLUSH` was negotiated — the device honours
    /// `VIRTIO_BLK_T_FLUSH` and a flush is required for true durability.
    flush_supported: bool,
}

impl VirtioBlkDevice {
    const fn new() -> Self {
        Self {
            queue: Virtqueue::new(),
            capacity_sectors: 0,
            ready: false,
            flush_supported: false,
        }
    }
}

/// Lifecycle of one submitted request chain (slot in the request table).
///
/// ```text
/// Free ──submit──▶ InFlight ──IRQ harvest──▶ Complete ──waiter collects──▶ Free
///                     │                                                      ▲
///                  timeout                                                   │
///                     ▼                                                      │
///                  Orphaned ──IRQ harvest──▶ OrphanDone ──task-context reap──┘
/// ```
///
/// The `Orphaned` arm is the timeout-correctness core: the device still
/// owns the descriptor chain and the DMA pages, so neither may be reused
/// or freed until the completion is actually observed on the used ring.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    InFlight,
    Complete,
    Orphaned,
    OrphanDone,
}

/// One in-flight (or quarantined) request chain, keyed by its head
/// descriptor index — the `id` the device echoes in its used-ring entry.
struct RequestSlot {
    state: SlotState,
    /// Head descriptor index of the chain (used-ring completion key).
    head: u16,
    /// Every descriptor index of the chain, `descs[0] == head`.
    descs: [u16; 3],
    desc_count: u8,
    /// DMA pages owned by this request. Held by the slot — not the
    /// submitting function — so a timed-out request cannot free pages
    /// the device still has descriptors pointing at.
    buffers: Option<RequestBuffers>,
}

impl RequestSlot {
    const EMPTY: RequestSlot = RequestSlot {
        state: SlotState::Free,
        head: 0,
        descs: [0; 3],
        desc_count: 0,
        buffers: None,
    };
}

/// Transfer direction + data slice for one sector-aligned request.
/// Writes borrow the source immutably so callers never need a staging
/// copy of the payload.
enum Xfer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

/// Combined device + MMIO caps + interrupt state under a single lock,
/// ensuring ownership/claim state and the request path share one coherent
/// synchronization model.
#[derive(slopos_ostd::SlotFields)]
struct VirtioBlkState {
    device: VirtioBlkDevice,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
    /// In-flight / quarantined request chains, keyed by head descriptor.
    slots: [RequestSlot; NUM_REQUEST_SLOTS],
}

impl VirtioBlkState {
    /// In-place recipe for the empty (pre-probe) state. Written field by
    /// field into the heap slot so the ~280-byte aggregate never lands on
    /// the prober's stack (the 2 KiB frame gate).
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, device, VirtioBlkDevice::new());
                write_field!(slot, caps, VirtioMmioCaps::empty());
                write_field!(slot, msix_state, None);
                write_field!(slot, slots, [RequestSlot::EMPTY; NUM_REQUEST_SLOTS]);
                Ok(slot.finish())
            },
        )
    }

    /// Consume every pending used-ring entry, transitioning the matching
    /// slot by its chain-head id. Runs in IRQ context (under the state
    /// `SpinLock`) and opportunistically from task context (wait
    /// predicate, timeout path, pre-scheduler poll), so a lost or
    /// spurious interrupt can delay but never desynchronize completion
    /// tracking: entries with an unknown id are dropped harmlessly
    /// instead of being attributed to whatever request is waiting.
    fn harvest_used(&mut self) {
        while let Some(elem) = self.device.queue.try_pop_used() {
            let head = elem.id as u16;
            let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| !matches!(s.state, SlotState::Free) && s.head == head)
            else {
                // Spurious or duplicate completion — virtio drivers must
                // tolerate these; nothing to attribute it to.
                continue;
            };
            slot.state = match slot.state {
                SlotState::InFlight => SlotState::Complete,
                SlotState::Orphaned => SlotState::OrphanDone,
                // Duplicate id against an already-completed chain: leave
                // the slot alone.
                other => other,
            };
        }
    }

    /// Release descriptors + DMA pages of every orphaned chain whose
    /// completion has since been harvested. Task context only (frees
    /// pages back to the allocator).
    fn reap_finished_orphans(&mut self) {
        for i in 0..NUM_REQUEST_SLOTS {
            if self.slots[i].state != SlotState::OrphanDone {
                continue;
            }
            let descs = self.slots[i].descs;
            let count = self.slots[i].desc_count as usize;
            for &d in &descs[..count] {
                self.device.queue.free_desc(d);
            }
            self.slots[i].buffers = None;
            self.slots[i] = RequestSlot::EMPTY;
        }
    }

    /// Collect a `Complete` slot: free its descriptors and hand the DMA
    /// pages (with the device-written status byte) back to the waiter.
    fn try_collect(&mut self, slot_idx: usize) -> Option<RequestBuffers> {
        if self.slots[slot_idx].state != SlotState::Complete {
            return None;
        }
        let descs = self.slots[slot_idx].descs;
        let count = self.slots[slot_idx].desc_count as usize;
        for &d in &descs[..count] {
            self.device.queue.free_desc(d);
        }
        let buffers = self.slots[slot_idx].buffers.take();
        self.slots[slot_idx] = RequestSlot::EMPTY;
        buffers
    }
}

/// Owned per-device state for one claimed virtio-blk device.
///
/// Lives on the heap inside a [`KArc`] so its address is stable: the
/// per-device IRQ closure and the registry both hold clones, and the
/// closure harvests the used ring + wakes `req_waiters` from interrupt
/// context. Replacing the former global statics (`VIRTIO_BLK_STATE` /
/// `BLK_QUEUE_EVENT` / `BLK_REQUEST_IN_FLIGHT`) with per-device fields
/// is what makes multi-device ownership — and thus exclusive write
/// claims — possible.
#[derive(slopos_ostd::SlotFields)]
struct VirtioBlkInner {
    /// Device + caps + MSI-X state + request slots. `LOCK_LEVEL_RESOURCE`.
    /// `SpinLock` disables IRQs while held, so the IRQ-side harvest and
    /// the task-side submit/collect paths never interleave mid-update.
    state: SpinLock<VirtioBlkState>,
    /// Serializes logical requests (one at a time per device). A sleeping
    /// mutex: a contender deschedules instead of spinning, so a slow
    /// request never monopolizes a CPU.
    io_lock: Mutex<()>,
    /// Waiters parked for request completion; woken by the IRQ handler
    /// after it harvests the used ring. Scheduler-backed — the waiting
    /// task deschedules and its CPU keeps running other work (the old
    /// `CompletionEvent` HLT-poll parked the whole CPU for up to 5 s,
    /// which is how a stuck flush froze the entire system).
    req_waiters: WaitQueue,
}

impl VirtioBlkInner {
    /// In-place recipe for a fresh, empty device. Built via
    /// [`KArc::try_init`] so neither the `VirtioBlkState` nor the
    /// surrounding `KArc` inner ever materialises on the caller's stack.
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    state,
                    SpinLock::init_with(LOCK_LEVEL_RESOURCE, VirtioBlkState::init_empty())
                )?;
                write_field!(slot, io_lock, Mutex::new(()));
                write_field!(slot, req_waiters, WaitQueue::new());
                Ok(slot.finish())
            },
        )
    }

    /// IRQ-side completion path: harvest every pending used-ring entry
    /// into the slot table, then wake parked waiters. The wake runs
    /// after the state lock is dropped so the wait queue's internal
    /// lock never nests inside the device lock.
    fn handle_queue_irq(&self) {
        {
            let mut state = self.state.lock();
            state.harvest_used();
        }
        let _ = self.req_waiters.wake_all();
    }

    fn is_ready(&self) -> bool {
        self.state.lock().device.ready
    }

    fn capacity_bytes(&self) -> u64 {
        self.state.lock().device.capacity_sectors * SECTOR_SIZE
    }

    /// Whether `[offset, offset + len)` lies fully within the device.
    /// Rejects a span that runs past capacity — an out-of-range LBA would
    /// otherwise be handed to the device (which errors it), and the
    /// partial-sector read-modify-write paths would touch a sector index
    /// past the end. `checked_add` also rejects an `offset + len` that
    /// overflows `u64`.
    fn span_in_bounds(&self, offset: u64, len: usize) -> bool {
        offset
            .checked_add(len as u64)
            .is_some_and(|end| end <= self.capacity_bytes())
    }

    #[cfg(feature = "test-hooks")]
    fn msix_state(&self) -> Option<VirtioMsixState> {
        self.state.lock().msix_state.clone()
    }

    /// Build, submit and record one request chain under the state lock.
    /// `data` is `Some((len, write))` for a read/write (3-descriptor
    /// chain: header → bounce → status) or `None` for a flush
    /// (2-descriptor chain: header → status). The DMA pages move into
    /// the request slot so their lifetime is tied to device ownership,
    /// not to the caller's stack frame. Returns the slot index.
    fn submit_chain(&self, buffers: RequestBuffers, data: Option<(u32, bool)>) -> Option<usize> {
        let req_phys = buffers.req_page.phys_u64();
        let status_offset = size_of::<VirtioBlkReqHeader>();
        let status_phys = req_phys + status_offset as u64;
        let bounce_phys = buffers.bounce_page.phys_u64();
        let desc_count: usize = if data.is_some() { 3 } else { 2 };

        let mut state = self.state.lock();
        if !state.device.queue.is_ready() {
            return None;
        }

        // Reclaim chains whose late completions have arrived since the
        // last request — keeps descriptor exhaustion bounded to chains
        // the device genuinely still owns.
        state.reap_finished_orphans();

        let slot_idx = state
            .slots
            .iter()
            .position(|s| s.state == SlotState::Free)?;

        let mut descs = [0u16; 3];
        for i in 0..desc_count {
            match state.device.queue.alloc_desc() {
                Some(d) => descs[i] = d,
                None => {
                    for &d in &descs[..i] {
                        state.device.queue.free_desc(d);
                    }
                    klog_info!("virtio-blk: descriptor ring exhausted (quarantined chains?)");
                    return None;
                }
            }
        }

        state.device.queue.write_desc(
            descs[0],
            VirtqDesc {
                addr: req_phys,
                len: size_of::<VirtioBlkReqHeader>() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: descs[1],
            },
        );
        match data {
            Some((len, write)) => {
                state.device.queue.write_desc(
                    descs[1],
                    VirtqDesc {
                        addr: bounce_phys,
                        len,
                        flags: if write {
                            VIRTQ_DESC_F_NEXT
                        } else {
                            VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
                        },
                        next: descs[2],
                    },
                );
                state.device.queue.write_desc(
                    descs[2],
                    VirtqDesc {
                        addr: status_phys,
                        len: 1,
                        flags: VIRTQ_DESC_F_WRITE,
                        next: 0,
                    },
                );
            }
            None => {
                state.device.queue.write_desc(
                    descs[1],
                    VirtqDesc {
                        addr: status_phys,
                        len: 1,
                        flags: VIRTQ_DESC_F_WRITE,
                        next: 0,
                    },
                );
            }
        }

        state.slots[slot_idx] = RequestSlot {
            state: SlotState::InFlight,
            head: descs[0],
            descs,
            desc_count: desc_count as u8,
            buffers: Some(buffers),
        };

        state.device.queue.submit(descs[0]);
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.device.queue,
            0,
        );
        Some(slot_idx)
    }

    /// Park until the chain in `slot_idx` completes, then return its DMA
    /// pages (carrying the device-written status byte). Scheduler-backed:
    /// the task deschedules and the IRQ-side harvest + wake delivers the
    /// completion. The predicate re-harvests opportunistically, so even a
    /// lost interrupt is recovered on the next wake or 500 ms wait slice.
    ///
    /// On timeout the chain is quarantined (`Orphaned`): its descriptors
    /// and pages remain reserved until the device's late completion is
    /// harvested — never freed or reused while the device may still DMA
    /// into them.
    fn wait_for_completion(&self, slot_idx: usize) -> Option<RequestBuffers> {
        let collect = || {
            let mut state = self.state.lock();
            state.harvest_used();
            state.try_collect(slot_idx)
        };

        match self
            .req_waiters
            .wait_event_timeout_until(collect, REQUEST_TIMEOUT_MS as u64)
        {
            WaitOutcome::Ready(buffers) => Some(buffers),
            WaitOutcome::NoRuntime => {
                // Pre-scheduler context (probe / early boot): poll the
                // used ring directly under the HPET deadline.
                virtio::hpet_poll_wait(
                    &|| {
                        let mut state = self.state.lock();
                        state.harvest_used();
                        state.slots[slot_idx].state == SlotState::Complete
                    },
                    REQUEST_TIMEOUT_MS,
                );
                self.finish_or_orphan(slot_idx)
            }
            WaitOutcome::Timeout => self.finish_or_orphan(slot_idx),
        }
    }

    /// Timeout epilogue: one final harvest (recovers a completion whose
    /// interrupt was lost), else quarantine the chain.
    fn finish_or_orphan(&self, slot_idx: usize) -> Option<RequestBuffers> {
        let mut state = self.state.lock();
        state.harvest_used();
        if let Some(buffers) = state.try_collect(slot_idx) {
            return Some(buffers);
        }
        if state.slots[slot_idx].state == SlotState::InFlight {
            state.slots[slot_idx].state = SlotState::Orphaned;
            klog_info!(
                "virtio-blk: request timeout — chain head {} quarantined until completion",
                state.slots[slot_idx].head
            );
        }
        None
    }

    /// Transfer whole sectors (at most one bounce page) starting at
    /// `sector`. The slice length must be a non-zero multiple of the
    /// sector size — callers chunk via [`read_offset`](Self::read_offset)
    /// / [`write_offset`](Self::write_offset). Writes stage through the
    /// bounce page directly from the caller's (immutably borrowed) slice.
    fn do_request(&self, sector: u64, xfer: Xfer<'_>) -> bool {
        let (len, write) = match &xfer {
            Xfer::Read(buf) => (buf.len(), false),
            Xfer::Write(buf) => (buf.len(), true),
        };
        if len == 0 || len > BOUNCE_CAPACITY || !len.is_multiple_of(SECTOR_SIZE as usize) {
            return false;
        }

        let _io = self.io_lock.lock();

        let buffers = match RequestBuffers::allocate() {
            Some(b) => b,
            None => return false,
        };

        if let Xfer::Write(buf) = &xfer
            && !buffers.bounce_page.write_slice(0, buf)
        {
            return false;
        }

        let header = VirtioBlkReqHeader {
            type_: if write {
                VIRTIO_BLK_T_OUT
            } else {
                VIRTIO_BLK_T_IN
            },
            reserved: 0,
            sector,
        };
        let status_offset = size_of::<VirtioBlkReqHeader>();
        if !buffers.req_page.write_at::<VirtioBlkReqHeader>(0, &header) {
            return false;
        }
        if !buffers
            .req_page
            .write_volatile_at::<u8>(status_offset, 0xFF)
        {
            return false;
        }

        let Some(slot_idx) = self.submit_chain(buffers, Some((len as u32, write))) else {
            return false;
        };
        let Some(buffers) = self.wait_for_completion(slot_idx) else {
            return false;
        };

        let status = buffers
            .req_page
            .read_volatile_at::<u8>(status_offset)
            .unwrap_or(0xFF);
        let success = status == VIRTIO_BLK_S_OK;

        if success
            && let Xfer::Read(buf) = xfer
            && !buffers.bounce_page.read_slice(0, buf)
        {
            return false;
        }

        success
    }

    /// Issue a `VIRTIO_BLK_T_FLUSH` and block until the device acknowledges it,
    /// forcing every previously-ACKed write out of the device's volatile cache
    /// onto non-volatile media. This is the durability barrier the filesystem
    /// relies on between ordered phases and at `sync`/shutdown.
    ///
    /// If `VIRTIO_BLK_F_FLUSH` was not negotiated the device has no volatile
    /// cache to flush (writes are already durable on ACK), so this is a
    /// successful no-op. The request is a 2-descriptor chain — header + status,
    /// no data phase — distinct from `do_request`'s 3-descriptor layout.
    fn do_flush(&self) -> bool {
        let _io = self.io_lock.lock();

        {
            let state = self.state.lock();
            if !state.device.queue.is_ready() {
                return false;
            }
            if !state.device.flush_supported {
                // No write-back cache advertised: ACKed writes are already
                // durable, so a flush is vacuously satisfied.
                return true;
            }
        }

        let buffers = match RequestBuffers::allocate() {
            Some(b) => b,
            None => return false,
        };

        let header = VirtioBlkReqHeader {
            type_: VIRTIO_BLK_T_FLUSH,
            reserved: 0,
            // The flush sector field must be zero per the virtio spec.
            sector: 0,
        };
        let status_offset = size_of::<VirtioBlkReqHeader>();
        if !buffers.req_page.write_at::<VirtioBlkReqHeader>(0, &header) {
            return false;
        }
        if !buffers
            .req_page
            .write_volatile_at::<u8>(status_offset, 0xFF)
        {
            return false;
        }

        // No data descriptor for a flush — header + status only.
        let Some(slot_idx) = self.submit_chain(buffers, None) else {
            return false;
        };
        let Some(buffers) = self.wait_for_completion(slot_idx) else {
            klog_info!("virtio-blk: flush did not complete");
            return false;
        };

        let status = buffers
            .req_page
            .read_volatile_at::<u8>(status_offset)
            .unwrap_or(0xFF);
        status == VIRTIO_BLK_S_OK
    }

    /// Byte-granular read: partial head/tail sectors go through a small
    /// stack staging buffer; the sector-aligned middle is transferred in
    /// chains of up to [`MAX_DATA_SECTORS`] sectors directly between the
    /// bounce page and the caller's slice (an exec-sized read is a
    /// handful of requests instead of one per sector).
    fn read_offset(&self, offset: u64, buffer: &mut [u8]) -> bool {
        if buffer.is_empty() {
            return true;
        }
        if !self.is_ready() {
            return false;
        }
        if !self.span_in_bounds(offset, buffer.len()) {
            return false;
        }

        let mut pos = 0usize;
        let mut cur = offset;

        // Partial head sector.
        let head_within = (cur % SECTOR_SIZE) as usize;
        if head_within != 0 {
            let mut sector_buf = [0u8; SECTOR_SIZE as usize];
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Read(&mut sector_buf)) {
                return false;
            }
            let n = (SECTOR_SIZE as usize - head_within).min(buffer.len());
            buffer[..n].copy_from_slice(&sector_buf[head_within..head_within + n]);
            pos += n;
            cur += n as u64;
        }

        // Sector-aligned middle, batched.
        while buffer.len() - pos >= SECTOR_SIZE as usize {
            let sectors = ((buffer.len() - pos) / SECTOR_SIZE as usize).min(MAX_DATA_SECTORS);
            let n = sectors * SECTOR_SIZE as usize;
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Read(&mut buffer[pos..pos + n])) {
                return false;
            }
            pos += n;
            cur += n as u64;
        }

        // Partial tail sector.
        if pos < buffer.len() {
            let mut sector_buf = [0u8; SECTOR_SIZE as usize];
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Read(&mut sector_buf)) {
                return false;
            }
            let n = buffer.len() - pos;
            buffer[pos..].copy_from_slice(&sector_buf[..n]);
        }

        true
    }

    /// Byte-granular write, mirror of [`read_offset`](Self::read_offset):
    /// partial head/tail sectors are read-modify-written through a stack
    /// staging buffer so bytes outside the span are never clobbered; the
    /// aligned middle is written in chains of up to [`MAX_DATA_SECTORS`].
    fn write_offset(&self, offset: u64, buffer: &[u8]) -> bool {
        if buffer.is_empty() {
            return true;
        }
        if !self.is_ready() {
            return false;
        }
        if !self.span_in_bounds(offset, buffer.len()) {
            return false;
        }

        let mut pos = 0usize;
        let mut cur = offset;

        // Partial head sector: read-modify-write.
        let head_within = (cur % SECTOR_SIZE) as usize;
        if head_within != 0 {
            let mut sector_buf = [0u8; SECTOR_SIZE as usize];
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Read(&mut sector_buf)) {
                return false;
            }
            let n = (SECTOR_SIZE as usize - head_within).min(buffer.len());
            sector_buf[head_within..head_within + n].copy_from_slice(&buffer[..n]);
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Write(&sector_buf)) {
                return false;
            }
            pos += n;
            cur += n as u64;
        }

        // Sector-aligned middle, batched zero-copy from the caller's slice.
        while buffer.len() - pos >= SECTOR_SIZE as usize {
            let sectors = ((buffer.len() - pos) / SECTOR_SIZE as usize).min(MAX_DATA_SECTORS);
            let n = sectors * SECTOR_SIZE as usize;
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Write(&buffer[pos..pos + n])) {
                return false;
            }
            pos += n;
            cur += n as u64;
        }

        // Partial tail sector: read-modify-write.
        if pos < buffer.len() {
            let mut sector_buf = [0u8; SECTOR_SIZE as usize];
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Read(&mut sector_buf)) {
                return false;
            }
            let n = buffer.len() - pos;
            sector_buf[..n].copy_from_slice(&buffer[pos..]);
            if !self.do_request(cur / SECTOR_SIZE, Xfer::Write(&sector_buf)) {
                return false;
            }
        }

        true
    }
}

struct RequestBuffers {
    req_page: OwnedPageFrame,
    bounce_page: OwnedPageFrame,
}

impl RequestBuffers {
    fn allocate() -> Option<Self> {
        let req_page = OwnedPageFrame::alloc_zeroed()?;
        let bounce_page = OwnedPageFrame::alloc_zeroed()?;
        Some(Self {
            req_page,
            bounce_page,
        })
    }
}

// ============================================================================
// Device registry + capability handles
//
// The registry owns every claimed virtio-blk device and is the ONLY way to
// reach one: there is no ambient "write any LBA" free function. Code obtains
// a device by index, then either reads through a borrowed handle or acquires
// an EXCLUSIVE write capability ([`open_writer`]) — modelled on Linux's
// `bd_writers` / FreeBSD GEOM exclusive-access counts. A second writer claim
// is rejected, so a buggy caller (or a test) cannot silently write a device
// the filesystem already owns.
// ============================================================================

const MAX_BLK_DEVICES: usize = 8;

/// Opaque, unforgeable handle to a registered virtio-blk device: a
/// generation-checked [`Handle`] over the registry's [`DevState`] slots, so a
/// stale handle fails validation instead of aliasing a different device. Held
/// purely in-kernel (never packed into an fd), so no encoding is needed.
pub type DevHandle = Handle<DevState>;

/// Error from acquiring an exclusive write capability on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkClaimError {
    /// The handle does not refer to a live device (freed/never existed).
    Stale,
    /// Another holder already owns the exclusive write capability.
    AlreadyClaimed,
}

/// One registered block device. Slot index and generation are owned by the
/// [`HandleTable`]; this carries the device state plus its capability bits.
/// Public as the type parameter of [`DevHandle`]; its fields stay private.
pub struct DevState {
    inner: KArc<VirtioBlkInner>,
    /// Stable probe-order index (disk0 = first probed). Devices are never
    /// removed, so this equals the device's position in registration order.
    index: u16,
    /// Whether a [`BlockWriteToken`] currently holds the exclusive write claim.
    write_claimed: bool,
}

static BLK_REGISTRY: SpinLock<Option<HandleTable<DevState>>> =
    SpinLock::new(None, LOCK_LEVEL_REGISTRY);

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<DevState>) -> R) -> R {
    let mut guard = BLK_REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_BLK_DEVICES).expect("blk registry alloc")
    });
    f(table)
}

/// Register a freshly-probed device. Assigns the next probe-order index
/// (disk0, disk1, …) — devices are never removed, so the live count is the
/// next index — and returns it. Called once per device from
/// `virtio_blk_probe` while the PCI `ENUM_STATE` lock is held; the only
/// nesting is `ENUM_STATE -> BLK_REGISTRY` (never the reverse), so no cycle.
/// Callers reach the device afterwards via [`blk_device_by_index`].
fn register_device(inner: KArc<VirtioBlkInner>) -> Option<BlockDeviceIndex> {
    with_registry(|t| {
        let index = t.len() as u16;
        t.insert(DevState {
            inner,
            index,
            write_claimed: false,
        })
        .ok()
        .map(|_| BlockDeviceIndex(index))
    })
}

/// Clone the device's owned state out from under the registry lock, so the
/// (potentially blocking) I/O path never runs while holding the registry.
fn clone_inner(handle: DevHandle) -> Option<KArc<VirtioBlkInner>> {
    with_registry(|t| t.get(handle).map(|s| s.inner.clone()).ok())
}

/// Number of claimed virtio-blk devices.
pub fn blk_device_count() -> usize {
    with_registry(|t| t.len())
}

/// Look a device up by stable probe-order index (disk0 = first probed).
pub fn blk_device_by_index(index: BlockDeviceIndex) -> Option<DevHandle> {
    with_registry(|t| t.iter().find(|(_, s)| s.index == index.0).map(|(h, _)| h))
}

/// Read-only block access through a borrowed handle (no exclusivity needed).
pub fn blk_read(handle: DevHandle, offset: u64, buffer: &mut [u8]) -> bool {
    clone_inner(handle).is_some_and(|inner| inner.read_offset(offset, buffer))
}

/// Whether the device is probed and ready.
pub fn blk_is_ready(handle: DevHandle) -> bool {
    clone_inner(handle).is_some_and(|inner| inner.is_ready())
}

/// Device capacity in bytes (0 if the handle is stale).
pub fn blk_capacity(handle: DevHandle) -> u64 {
    clone_inner(handle).map_or(0, |inner| inner.capacity_bytes())
}

#[cfg(feature = "test-hooks")]
pub fn blk_msix_state(handle: DevHandle) -> Option<VirtioMsixState> {
    clone_inner(handle).and_then(|inner| inner.msix_state())
}

/// Acquire the EXCLUSIVE write capability for a device. Returns an owned
/// [`BlockWriteToken`]; a second `open_writer` on the same device returns
/// [`BlkClaimError::AlreadyClaimed`] until the first token is dropped.
pub fn open_writer(handle: DevHandle) -> Result<BlockWriteToken, BlkClaimError> {
    with_registry(|t| match t.get_mut(handle) {
        Ok(s) if s.write_claimed => Err(BlkClaimError::AlreadyClaimed),
        Ok(s) => {
            let inner = s.inner.clone();
            s.write_claimed = true;
            Ok(BlockWriteToken { handle, inner })
        }
        Err(_) => Err(BlkClaimError::Stale),
    })
}

/// Owned, exclusive read+write capability for one block device. Implements
/// [`BlockDevice`] so the filesystem holds it as its sole writable handle.
/// Dropping it releases the exclusive write claim.
pub struct BlockWriteToken {
    handle: DevHandle,
    inner: KArc<VirtioBlkInner>,
}

impl BlockDevice for BlockWriteToken {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if self.inner.read_offset(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if self.inner.write_offset(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        if self.inner.do_flush() {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }
}

impl Drop for BlockWriteToken {
    fn drop(&mut self) {
        with_registry(|t| {
            if let Ok(s) = t.get_mut(self.handle) {
                s.write_claimed = false;
            }
        });
    }
}

fn read_capacity(caps: &VirtioMmioCaps) -> u64 {
    if !caps.has_device_cfg() {
        return 0;
    }
    let lo = caps.device_cfg.read::<u32>(0) as u64;
    let hi = caps.device_cfg.read::<u32>(4) as u64;
    lo | (hi << 32)
}

fn virtio_blk_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    klog_info!(
        "virtio-blk: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(&info);

    let caps = parse_capabilities(&info);

    klog_debug!(
        "virtio-blk: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        return Err(PciProbeError::Unsupported);
    }

    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, VIRTIO_BLK_F_FLUSH);
    if !feat_result.success {
        klog_info!("virtio-blk: features negotiation failed");
        return Err(PciProbeError::DeviceFault);
    }
    let flush_supported = feat_result.driver_features & VIRTIO_BLK_F_FLUSH != 0;

    // Allocate the per-device state up front (empty) so the IRQ closure can
    // capture a KArc clone and signal THIS device's completion event.
    let inner = match KArc::try_init(VirtioBlkInner::init_empty()) {
        Ok(i) => i,
        Err(_) => return Err(PciProbeError::OutOfMemory),
    };

    // --- MSI-X / MSI interrupt setup ---
    // VirtIO modern on q35 always has MSI-X; MSI is the minimum fallback.
    // The handler is a per-device closure that harvests this device's own
    // used ring and wakes its parked waiters — no global IRQ sink.
    let inner_for_irq = inner.clone();
    let (irq_mode, msix_state) = setup_interrupts(bound, &caps, 1, move |_q: u8| {
        inner_for_irq.handle_queue_irq();
    })
    .unwrap_or_else(|msg| {
        panic!(
            "virtio-blk: {}:{}.{} {}",
            info.bus, info.device, info.function, msg
        )
    });
    let q0_msix_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));

    let capacity_sectors;
    {
        // The queue is set up in place inside the heap-resident state so
        // the ~200-byte `Virtqueue` never lands on this probe's stack
        // frame (2 KiB frame gate).
        let mut state = inner.state.lock();
        if !queue::setup_queue_into(
            &caps.common_cfg,
            0,
            DEFAULT_QUEUE_SIZE,
            q0_msix_entry,
            &mut state.device.queue,
        ) {
            klog_info!("virtio-blk: queue setup failed");
            return Err(PciProbeError::OutOfMemory);
        }

        set_driver_ok(&caps);

        capacity_sectors = read_capacity(&caps);
        state.device.capacity_sectors = capacity_sectors;
        state.device.flush_supported = flush_supported;
        state.device.ready = true;
        state.caps = caps;
        state.msix_state = msix_state;
    }

    let index = match register_device(inner) {
        Some(i) => i,
        None => {
            klog_info!("virtio-blk: device registry full");
            return Err(PciProbeError::OutOfMemory);
        }
    };

    klog_info!(
        "virtio-blk: disk{} ready, capacity {} sectors ({} MB), flush={}, irq {:?}",
        index.0,
        capacity_sectors,
        (capacity_sectors * SECTOR_SIZE) / (1024 * 1024),
        flush_supported,
        irq_mode,
    );

    Ok(ProbeOutcome::Bound)
}

crate::pci_driver! {
    pub static VIRTIO_BLK_DRIVER = {
        name: "virtio-blk",
        match_table: &[
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_LEGACY,
            },
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_MODERN,
            },
        ],
        probe: virtio_blk_probe,
    };
}

// All block access now flows through the capability registry above
// (`blk_device_by_index` + `open_writer`/`blk_read`/`blk_is_ready` /
// `blk_capacity` / `blk_msix_state`). There is no ambient "read/write any
// LBA on the global device" free function: that surface was the root of the
// io_capture on-disk corruption and has been removed.
