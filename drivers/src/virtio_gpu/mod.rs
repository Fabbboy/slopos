//! VirtIO GPU 2D driver: a scanout-backed framebuffer presented via
//! `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`, plus a hardware cursor overlay and
//! runtime mode-set.
//!
//! The control queue (index 0) carries resource/scanout/transfer/flush commands
//! and their responses, the cursor queue (index 1) `UPDATE_CURSOR`/`MOVE_CURSOR`;
//! `ctrl_lock`/`cursor_lock` keep at most one command per queue in flight.

mod protocol;

use core::ffi::c_int;
use core::mem::size_of;
use slopos_ostd::lock_class;

use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_abi::{DisplayInfo, FramebufferData, PixelFormat};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::page_alloc::{OwnedPageFrame, alloc_kernel_pages, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::KArc;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::WaitAbort;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, Mutex, SpinLock, WaitQueue};
use slopos_ostd::util::ptr_buf;
use slopos_ostd::{klog_info, klog_warn, write_field, write_init_field};

use slopos_kernel_services::syscall_services::scanout::{
    self, ClaimOutcome, GpuControlFns, InstallCtx, ScanoutId, ScanoutProvider,
};

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

use protocol::*;

/// Generous; QEMU acks GPU commands in microseconds.
const CMD_TIMEOUT_MS: u32 = 2000;
/// The command occupies `[0, RESP_OFFSET)` of the 4 KiB DMA page, the
/// device-written response `[RESP_OFFSET, 4096)`.
const RESP_OFFSET: usize = 2048;
/// One command is in flight per queue; the extra slot absorbs a chain
/// quarantined by a timeout.
const NUM_GPU_SLOTS: usize = 2;
/// Hardware cursor dimensions mandated by virtio-gpu.
const CURSOR_W: u32 = 64;
const CURSOR_H: u32 = 64;

/// The scanout ping-pongs between PRIMARY and ALT on mode-set so the new
/// scanout is live before the old resource is unref'd.
const RES_FB_PRIMARY: u32 = 1;
const RES_CURSOR: u32 = 2;
const RES_FB_ALT: u32 = 3;

const PAGE_SIZE: usize = PAGE_SIZE_4KB as usize;

const PRESENT_XFER_CMD: usize = 0;
const PRESENT_XFER_RESP: usize = 1024;
const PRESENT_FLUSH_CMD: usize = 2048;
const PRESENT_FLUSH_RESP: usize = 3072;
const PRESENT_NONE: u16 = u16::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    InFlight,
    Complete,
    Orphaned,
    OrphanDone,
}

/// Keyed by head descriptor — the id the device echoes on the used ring. Owns
/// the DMA page so a timed-out command never frees memory the device may still
/// read/write.
struct GpuSlot {
    state: SlotState,
    head: u16,
    descs: [u16; 2],
    desc_count: u8,
    page: Option<OwnedPageFrame>,
}

impl GpuSlot {
    const EMPTY: GpuSlot = GpuSlot {
        state: SlotState::Free,
        head: 0,
        descs: [0; 2],
        desc_count: 0,
        page: None,
    };
}

struct GpuQueue {
    q: Virtqueue,
    index: u16,
    slots: [GpuSlot; NUM_GPU_SLOTS],
    /// Fire-and-forget present engine (control queue only): a pre-allocated
    /// command page and two chain heads submitted without waiting, so `present`
    /// neither allocates nor blocks.
    present_page: Option<OwnedPageFrame>,
    present_busy: bool,
    present_heads: [u16; 2],
    present_descs: [[u16; 2]; 2],
    present_remaining: u8,
}

impl GpuQueue {
    const fn new(index: u16) -> Self {
        Self {
            q: Virtqueue::new(),
            index,
            slots: [GpuSlot::EMPTY; NUM_GPU_SLOTS],
            present_page: None,
            present_busy: false,
            present_heads: [PRESENT_NONE; 2],
            present_descs: [[0; 2]; 2],
            present_remaining: 0,
        }
    }

    fn complete_present_head(&mut self, head: u16) -> bool {
        for i in 0..2 {
            if self.present_heads[i] != PRESENT_NONE && self.present_heads[i] == head {
                for &d in &self.present_descs[i] {
                    self.q.free_desc(d);
                }
                self.present_heads[i] = PRESENT_NONE;
                self.present_remaining = self.present_remaining.saturating_sub(1);
                if self.present_remaining == 0 {
                    self.present_busy = false;
                }
                return true;
            }
        }
        false
    }

    /// `false` means the frame was dropped: a present is still in flight, the
    /// page is unset, or descriptors are exhausted. IRQ-safe: no alloc.
    fn submit_present(&mut self, xfer_len: u32, flush_len: u32) -> bool {
        if !self.q.is_ready() || self.present_busy {
            return false;
        }
        let Some(phys) = self.present_page.as_ref().map(|p| p.phys_u64()) else {
            return false;
        };
        let mut d = [0u16; 4];
        for i in 0..4 {
            match self.q.alloc_desc() {
                Some(x) => d[i] = x,
                None => {
                    for &x in &d[..i] {
                        self.q.free_desc(x);
                    }
                    return false;
                }
            }
        }
        let resp_len = size_of::<VirtioGpuCtrlHdr>() as u32;
        self.q.write_desc(
            d[0],
            VirtqDesc {
                addr: phys + PRESENT_XFER_CMD as u64,
                len: xfer_len,
                flags: VIRTQ_DESC_F_NEXT,
                next: d[1],
            },
        );
        self.q.write_desc(
            d[1],
            VirtqDesc {
                addr: phys + PRESENT_XFER_RESP as u64,
                len: resp_len,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        self.q.write_desc(
            d[2],
            VirtqDesc {
                addr: phys + PRESENT_FLUSH_CMD as u64,
                len: flush_len,
                flags: VIRTQ_DESC_F_NEXT,
                next: d[3],
            },
        );
        self.q.write_desc(
            d[3],
            VirtqDesc {
                addr: phys + PRESENT_FLUSH_RESP as u64,
                len: resp_len,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        self.present_heads = [d[0], d[2]];
        self.present_descs = [[d[0], d[1]], [d[2], d[3]]];
        self.present_remaining = 2;
        self.present_busy = true;
        self.q.submit(d[0]);
        self.q.submit(d[2]);
        true
    }

    fn harvest(&mut self) {
        while let Some(elem) = self.q.try_pop_used() {
            let head = elem.id as u16;
            if self.present_busy && self.complete_present_head(head) {
                continue;
            }
            let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.state != SlotState::Free && s.head == head)
            else {
                continue;
            };
            slot.state = match slot.state {
                SlotState::InFlight => SlotState::Complete,
                SlotState::Orphaned => SlotState::OrphanDone,
                other => other,
            };
        }
    }

    fn reap_orphans(&mut self) {
        for i in 0..NUM_GPU_SLOTS {
            if self.slots[i].state != SlotState::OrphanDone {
                continue;
            }
            let descs = self.slots[i].descs;
            let count = self.slots[i].desc_count as usize;
            for &d in &descs[..count] {
                self.q.free_desc(d);
            }
            self.slots[i] = GpuSlot::EMPTY;
        }
    }

    fn try_collect(&mut self, slot_idx: usize) -> Option<OwnedPageFrame> {
        if self.slots[slot_idx].state != SlotState::Complete {
            return None;
        }
        let descs = self.slots[slot_idx].descs;
        let count = self.slots[slot_idx].desc_count as usize;
        for &d in &descs[..count] {
            self.q.free_desc(d);
        }
        let page = self.slots[slot_idx].page.take();
        self.slots[slot_idx] = GpuSlot::EMPTY;
        page
    }

    /// `segs` is `(phys, len, extra_flags)`; the last segment terminates the
    /// chain. `page` moves into the slot, tying its lifetime to device ownership.
    fn submit_descs(&mut self, page: OwnedPageFrame, segs: &[(u64, u32, u16)]) -> Option<usize> {
        if !self.q.is_ready() {
            return None;
        }
        self.reap_orphans();

        let n = segs.len();
        if n == 0 || n > 2 {
            return None;
        }
        let slot_idx = self.slots.iter().position(|s| s.state == SlotState::Free)?;

        let mut descs = [0u16; 2];
        for i in 0..n {
            match self.q.alloc_desc() {
                Some(d) => descs[i] = d,
                None => {
                    for &d in &descs[..i] {
                        self.q.free_desc(d);
                    }
                    return None;
                }
            }
        }

        for i in 0..n {
            let (addr, len, extra) = segs[i];
            let last = i + 1 == n;
            self.q.write_desc(
                descs[i],
                VirtqDesc {
                    addr,
                    len,
                    flags: extra | if last { 0 } else { VIRTQ_DESC_F_NEXT },
                    next: if last { 0 } else { descs[i + 1] },
                },
            );
        }

        self.slots[slot_idx] = GpuSlot {
            state: SlotState::InFlight,
            head: descs[0],
            descs,
            desc_count: n as u8,
            page: Some(page),
        };
        self.q.submit(descs[0]);
        Some(slot_idx)
    }
}

#[derive(Clone, Copy)]
enum QSel {
    Control,
    Cursor,
}

#[derive(Clone, Copy)]
struct ScanoutGeom {
    ready: bool,
    resource_id: u32,
    backing_phys: PhysAddr,
    width: u32,
    height: u32,
    pitch: u32,
    sl_format: PixelFormat,
}

impl ScanoutGeom {
    const fn empty() -> Self {
        Self {
            ready: false,
            resource_id: 0,
            backing_phys: PhysAddr::NULL,
            width: 0,
            height: 0,
            pitch: 0,
            sl_format: PixelFormat::Argb8888,
        }
    }
}

#[derive(Clone, Copy)]
struct CursorState {
    created: bool,
    resource_id: u32,
    backing_phys: PhysAddr,
    hot_x: u32,
    hot_y: u32,
    /// Last commanded position; `UPDATE_CURSOR` reuses it so a shape-change
    /// re-upload does not yank the cursor to the origin.
    x: u32,
    y: u32,
}

impl CursorState {
    const fn empty() -> Self {
        Self {
            created: false,
            resource_id: RES_CURSOR,
            backing_phys: PhysAddr::NULL,
            hot_x: 0,
            hot_y: 0,
            x: 0,
            y: 0,
        }
    }
}

#[derive(slopos_ostd::SlotFields)]
struct VirtioGpuState {
    control: GpuQueue,
    cursor: GpuQueue,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
    ready: bool,
    num_scanouts: u32,
    edid_supported: bool,
    geom: ScanoutGeom,
    cursor_state: CursorState,
    /// Backing of the previous scanout, held until the mode after next so a
    /// consumer that still points at it cannot write into freed memory.
    retired_backing: PhysAddr,
}

impl VirtioGpuState {
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, control, GpuQueue::new(0));
                write_field!(slot, cursor, GpuQueue::new(1));
                write_field!(slot, caps, VirtioMmioCaps::empty());
                write_field!(slot, msix_state, None);
                write_field!(slot, ready, false);
                write_field!(slot, num_scanouts, 0u32);
                write_field!(slot, edid_supported, false);
                write_field!(slot, geom, ScanoutGeom::empty());
                write_field!(slot, cursor_state, CursorState::empty());
                write_field!(slot, retired_backing, PhysAddr::NULL);
                Ok(slot.finish())
            },
        )
    }
}

#[derive(slopos_ostd::SlotFields)]
struct VirtioGpuInner {
    /// IRQs are off while held, so the IRQ-side harvest and task-side
    /// submit/collect never interleave mid-update. Never held across a
    /// blocking wait.
    state: SpinLock<VirtioGpuState>,
    /// Serializes control-queue commands.
    ctrl_lock: Mutex<()>,
    /// Serializes cursor-queue commands.
    cursor_lock: Mutex<()>,
    ctrl_waiters: WaitQueue,
    cursor_waiters: WaitQueue,
}

impl VirtioGpuInner {
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    state,
                    SpinLock::init_with(
                        lock_class!("VirtioGpu.state", LOCK_LEVEL_RESOURCE),
                        VirtioGpuState::init_empty()
                    )
                )?;
                write_field!(
                    slot,
                    ctrl_lock,
                    Mutex::new((), lock_class!("VirtioGpu.ctrl_lock", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    slot,
                    cursor_lock,
                    Mutex::new(
                        (),
                        lock_class!("VirtioGpu.cursor_lock", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(
                    slot,
                    ctrl_waiters,
                    WaitQueue::new(lock_class!("VirtioGpu.ctrl_waiters", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    slot,
                    cursor_waiters,
                    WaitQueue::new(lock_class!("VirtioGpu.cursor_waiters", LOCK_LEVEL_RESOURCE))
                );
                Ok(slot.finish())
            },
        )
    }

    /// The shared MSI/MSI-X fallback delivers a single `q`, so both queues are
    /// harvested regardless of which interrupt fired.
    fn handle_queue_irq(&self, _q: u8) {
        {
            let mut st = self.state.lock();
            st.control.harvest();
            st.cursor.harvest();
        }
        let _ = self.ctrl_waiters.wake_all();
        let _ = self.cursor_waiters.wake_all();
    }

    fn submit_and_notify(
        &self,
        which: QSel,
        page: OwnedPageFrame,
        segs: &[(u64, u32, u16)],
    ) -> Option<usize> {
        let mut st = self.state.lock();
        let slot_idx = match which {
            QSel::Control => st.control.submit_descs(page, segs),
            QSel::Cursor => st.cursor.submit_descs(page, segs),
        }?;
        let mult = st.caps.notify_off_multiplier;
        match which {
            QSel::Control => {
                queue::notify_queue(&st.caps.notify_cfg, mult, &st.control.q, st.control.index)
            }
            QSel::Cursor => {
                queue::notify_queue(&st.caps.notify_cfg, mult, &st.cursor.q, st.cursor.index)
            }
        }
        Some(slot_idx)
    }

    /// The state lock is never held across the wait, so the GPU IRQ can be
    /// serviced.
    fn wait_completion(&self, which: QSel, slot_idx: usize) -> Option<OwnedPageFrame> {
        let collect = || {
            let mut st = self.state.lock();
            match which {
                QSel::Control => {
                    st.control.harvest();
                    st.control.try_collect(slot_idx)
                }
                QSel::Cursor => {
                    st.cursor.harvest();
                    st.cursor.try_collect(slot_idx)
                }
            }
        };
        let waiters = match which {
            QSel::Control => &self.ctrl_waiters,
            QSel::Cursor => &self.cursor_waiters,
        };
        match waiters.wait_event_timeout_until(collect, CMD_TIMEOUT_MS as u64) {
            Ok(page) => Some(page),
            Err(WaitAbort::NoRuntime) => {
                virtio::hpet_poll_wait(
                    &|| {
                        let mut st = self.state.lock();
                        let q = match which {
                            QSel::Control => &mut st.control,
                            QSel::Cursor => &mut st.cursor,
                        };
                        q.harvest();
                        q.slots[slot_idx].state == SlotState::Complete
                    },
                    CMD_TIMEOUT_MS,
                );
                self.finish_or_orphan(which, slot_idx)
            }
            Err(WaitAbort::Timeout | WaitAbort::Killed | WaitAbort::Interrupted) => {
                self.finish_or_orphan(which, slot_idx)
            }
        }
    }

    fn finish_or_orphan(&self, which: QSel, slot_idx: usize) -> Option<OwnedPageFrame> {
        let mut st = self.state.lock();
        let q = match which {
            QSel::Control => &mut st.control,
            QSel::Cursor => &mut st.cursor,
        };
        q.harvest();
        if let Some(page) = q.try_collect(slot_idx) {
            return Some(page);
        }
        if q.slots[slot_idx].state == SlotState::InFlight {
            q.slots[slot_idx].state = SlotState::Orphaned;
            klog_warn!("virtio-gpu: command timeout — chain quarantined");
        }
        None
    }

    /// `page` must already hold the command at offset 0; blocks until the
    /// device completes it.
    fn ctrl_submit_page(
        &self,
        page: OwnedPageFrame,
        cmd_len: u32,
        resp_len: u32,
    ) -> Option<OwnedPageFrame> {
        let Ok(_g) = self.ctrl_lock.lock() else {
            return None;
        };
        let phys = page.phys_u64();
        let segs = [
            (phys, cmd_len, 0u16),
            (phys + RESP_OFFSET as u64, resp_len, VIRTQ_DESC_F_WRITE),
        ];
        let slot_idx = self.submit_and_notify(QSel::Control, page, &segs)?;
        self.wait_completion(QSel::Control, slot_idx)
    }

    /// Single-descriptor cursor command (no response); blocks until the device
    /// reclaims the descriptor.
    fn cursor_submit_page(&self, page: OwnedPageFrame, cmd_len: u32) -> bool {
        let Ok(_g) = self.cursor_lock.lock() else {
            return false;
        };
        let phys = page.phys_u64();
        let segs = [(phys, cmd_len, 0u16)];
        match self.submit_and_notify(QSel::Cursor, page, &segs) {
            Some(slot_idx) => self.wait_completion(QSel::Cursor, slot_idx).is_some(),
            None => false,
        }
    }

    fn ctrl_cmd_nodata<C: slopos_ostd::Pod>(&self, cmd: &C) -> bool {
        let Some(page) = OwnedPageFrame::alloc_zeroed() else {
            return false;
        };
        if !page.write_at::<C>(0, cmd) {
            return false;
        }
        let Some(resp) = self.ctrl_submit_page(
            page,
            size_of::<C>() as u32,
            size_of::<VirtioGpuCtrlHdr>() as u32,
        ) else {
            return false;
        };
        resp.read_at::<VirtioGpuCtrlHdr>(RESP_OFFSET)
            .map(|h| is_ok_resp(h.type_))
            .unwrap_or(false)
    }

    fn resource_create_2d(&self, resource_id: u32, format: u32, width: u32, height: u32) -> bool {
        self.ctrl_cmd_nodata(&VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D),
            resource_id,
            format,
            width,
            height,
        })
    }

    fn resource_unref(&self, resource_id: u32) -> bool {
        self.ctrl_cmd_nodata(&VirtioGpuResourceUnref {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_RESOURCE_UNREF),
            resource_id,
            padding: 0,
        })
    }

    fn set_scanout(&self, scanout_id: u32, resource_id: u32, w: u32, h: u32) -> bool {
        self.ctrl_cmd_nodata(&VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_SET_SCANOUT),
            r: VirtioGpuRect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            scanout_id,
            resource_id,
        })
    }

    fn transfer_to_host(&self, resource_id: u32, rect: VirtioGpuRect, offset: u64) -> bool {
        self.ctrl_cmd_nodata(&VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D),
            r: rect,
            offset,
            resource_id,
            padding: 0,
        })
    }

    fn resource_flush(&self, resource_id: u32, rect: VirtioGpuRect) -> bool {
        self.ctrl_cmd_nodata(&VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_RESOURCE_FLUSH),
            r: rect,
            resource_id,
            padding: 0,
        })
    }

    fn attach_backing(&self, resource_id: u32, phys: u64, len: u32) -> bool {
        let Some(page) = OwnedPageFrame::alloc_zeroed() else {
            return false;
        };
        let cmd = VirtioGpuResourceAttachBacking {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING),
            resource_id,
            nr_entries: 1,
        };
        if !page.write_at(0, &cmd) {
            return false;
        }
        let entry = VirtioGpuMemEntry {
            addr: phys,
            length: len,
            padding: 0,
        };
        if !page.write_at(size_of::<VirtioGpuResourceAttachBacking>(), &entry) {
            return false;
        }
        let cmd_len =
            (size_of::<VirtioGpuResourceAttachBacking>() + size_of::<VirtioGpuMemEntry>()) as u32;
        let Some(resp) = self.ctrl_submit_page(page, cmd_len, size_of::<VirtioGpuCtrlHdr>() as u32)
        else {
            return false;
        };
        resp.read_at::<VirtioGpuCtrlHdr>(RESP_OFFSET)
            .map(|h| is_ok_resp(h.type_))
            .unwrap_or(false)
    }

    /// Host's configured size for scanout 0, if enabled.
    fn get_display_info(&self) -> Option<(u32, u32)> {
        let page = OwnedPageFrame::alloc_zeroed()?;
        if !page.write_at(0, &VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_GET_DISPLAY_INFO)) {
            return None;
        }
        let resp = self.ctrl_submit_page(
            page,
            size_of::<VirtioGpuCtrlHdr>() as u32,
            DISPLAY_INFO_RESP_LEN as u32,
        )?;
        let hdr: VirtioGpuCtrlHdr = resp.read_at(RESP_OFFSET)?;
        if hdr.type_ != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            return None;
        }
        let rect: VirtioGpuRect = resp.read_at(RESP_OFFSET + DISPLAY_INFO_PMODE0_RECT)?;
        let enabled: u32 = resp.read_at(RESP_OFFSET + DISPLAY_INFO_PMODE0_ENABLED)?;
        if enabled == 0 || rect.width == 0 || rect.height == 0 {
            return None;
        }
        Some((rect.width, rect.height))
    }

    /// Preferred resolution from the first detailed-timing descriptor, at byte
    /// 54 of the EDID base block.
    fn get_edid_mode(&self) -> Option<(u32, u32)> {
        let page = OwnedPageFrame::alloc_zeroed()?;
        let cmd = VirtioGpuGetEdid {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_GET_EDID),
            scanout: 0,
            padding: 0,
        };
        if !page.write_at(0, &cmd) {
            return None;
        }
        let resp = self.ctrl_submit_page(
            page,
            size_of::<VirtioGpuGetEdid>() as u32,
            EDID_RESP_LEN as u32,
        )?;
        let hdr: VirtioGpuCtrlHdr = resp.read_at(RESP_OFFSET)?;
        if hdr.type_ != VIRTIO_GPU_RESP_OK_EDID {
            return None;
        }
        let dtd = RESP_OFFSET + EDID_RESP_BLOB_OFFSET + 54;
        let b2: u8 = resp.read_at(dtd + 2)?;
        let b4: u8 = resp.read_at(dtd + 4)?;
        let b5: u8 = resp.read_at(dtd + 5)?;
        let b7: u8 = resp.read_at(dtd + 7)?;
        let hactive = (b2 as u32) | (((b4 as u32) & 0xF0) << 4);
        let vactive = (b5 as u32) | (((b7 as u32) & 0xF0) << 4);
        if hactive == 0 || vactive == 0 {
            return None;
        }
        Some((hactive, vactive))
    }

    fn choose_mode(&self, boot_fb: Option<FramebufferData>) -> (u32, u32) {
        if let Some(mode) = self
            .get_display_info()
            .and_then(|(w, h)| sanitize_mode(w, h))
        {
            return mode;
        }
        let edid_supported = self.state.lock().edid_supported;
        if edid_supported
            && let Some(mode) = self.get_edid_mode().and_then(|(w, h)| sanitize_mode(w, h))
        {
            return mode;
        }
        if let Some(bf) = boot_fb
            && let Some(mode) = sanitize_mode(bf.info.width, bf.info.height)
        {
            return mode;
        }
        (1280, 800)
    }

    /// Brings up scanout 0 on a fresh backing, seeded from the firmware
    /// framebuffer so the boot splash survives the takeover.
    ///
    /// `#[inline(never)]`: a one-shot probe-time path whose locals would
    /// otherwise land in `virtio_gpu_probe`'s frame, which the stack gate
    /// bounds.
    #[inline(never)]
    fn setup_scanout(&self, boot_fb: Option<FramebufferData>) -> Option<FramebufferData> {
        if self.state.lock().num_scanouts == 0 {
            klog_warn!("virtio-gpu: device reports zero scanouts");
            return None;
        }
        let (w, h) = self.choose_mode(boot_fb);
        let sl_format = boot_fb
            .map(|f| f.info.format)
            .unwrap_or(PixelFormat::Argb8888);
        let sl_format = match sl_format {
            PixelFormat::Argb8888 | PixelFormat::Xrgb8888 => sl_format,
            _ => PixelFormat::Xrgb8888,
        };
        let vfmt = format_from_pixel(sl_format);

        let (addr, phys, pitch, size) = self.alloc_backing(w, h)?;

        if !self.resource_create_2d(RES_FB_PRIMARY, vfmt, w, h) {
            free_page_frame(phys);
            return None;
        }
        // Past resource creation the device may hold a backing reference to
        // `phys`, so unref before the page returns to the allocator.
        if !self.attach_backing(RES_FB_PRIMARY, phys.as_u64(), size as u32)
            || !self.set_scanout(0, RES_FB_PRIMARY, w, h)
        {
            let _ = self.resource_unref(RES_FB_PRIMARY);
            free_page_frame(phys);
            return None;
        }

        if let Some(bf) = boot_fb
            && !bf.address.is_null()
        {
            let copy = bf.info.buffer_size().min(size);
            if copy > 0 {
                ptr_buf::copy_bytes(addr, bf.address as *const u8, copy);
            }
        }

        let full = VirtioGpuRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let _ = self.transfer_to_host(RES_FB_PRIMARY, full, 0);
        let _ = self.resource_flush(RES_FB_PRIMARY, full);

        // Allocated in task context so the per-frame present, which may run
        // from the `fblog` timer-tick interrupt, never allocates.
        let present_page = OwnedPageFrame::alloc_zeroed();

        {
            let mut st = self.state.lock();
            if st.control.present_page.is_none() {
                st.control.present_page = present_page;
            }
            st.geom = ScanoutGeom {
                ready: true,
                resource_id: RES_FB_PRIMARY,
                backing_phys: phys,
                width: w,
                height: h,
                pitch,
                sl_format,
            };
        }

        klog_info!(
            "virtio-gpu: scanout {}x{} pitch {} resource {}",
            w,
            h,
            pitch,
            RES_FB_PRIMARY
        );
        Some(FramebufferData {
            address: addr,
            info: DisplayInfo::new(w, h, pitch, sl_format),
        })
    }

    /// Returns (virt ptr, phys, pitch, size bytes) for a contiguous 32bpp
    /// `w`×`h` backing.
    fn alloc_backing(&self, w: u32, h: u32) -> Option<(*mut u8, PhysAddr, u32, usize)> {
        let pitch = w.checked_mul(4)?;
        let size = (pitch as usize).checked_mul(h as usize)?;
        let pages = size.div_ceil(PAGE_SIZE) as u32;
        let phys = alloc_kernel_pages(pages);
        if phys.is_null() {
            return None;
        }
        let Some(virt) = phys.to_virt_checked() else {
            free_page_frame(phys);
            return None;
        };
        Some((virt.as_u64() as *mut u8, phys, pitch, size))
    }

    /// Fire-and-forget present of the damage bounding box. Never blocks or
    /// allocates, so it is callable from the `fblog` timer tick as well as the
    /// compositor; a present still in flight drops this frame rather than
    /// waiting.
    fn present(&self, damage: *const DamageRect, count: u32) -> c_int {
        let mut st = self.state.lock();
        let geom = st.geom;
        if !geom.ready || geom.resource_id == 0 {
            return -1;
        }
        st.control.harvest();
        if st.control.present_busy {
            // 1 = suppressed: the compositor keeps the damage pending and
            // retries next frame.
            return 1;
        }
        if st.control.present_page.is_none() {
            return -1;
        }

        let rect = if damage.is_null() || count == 0 {
            VirtioGpuRect {
                x: 0,
                y: 0,
                width: geom.width,
                height: geom.height,
            }
        } else {
            let n = (count as usize).min(MAX_DAMAGE_REGIONS);
            let coalesced = ptr_buf::with_buf(damage, n, |regions| {
                coalesce_damage(regions, geom.width, geom.height)
            });
            match coalesced {
                Some(r) => r,
                None => return 0,
            }
        };
        let offset = (rect.y as u64) * (geom.pitch as u64) + (rect.x as u64) * 4;

        let xfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D),
            r: rect,
            offset,
            resource_id: geom.resource_id,
            padding: 0,
        };
        let flush = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_RESOURCE_FLUSH),
            r: rect,
            resource_id: geom.resource_id,
            padding: 0,
        };
        {
            let page = st.control.present_page.as_ref().unwrap();
            if !page.write_at(PRESENT_XFER_CMD, &xfer) || !page.write_at(PRESENT_FLUSH_CMD, &flush)
            {
                return -1;
            }
        }

        let xfer_len = size_of::<VirtioGpuTransferToHost2d>() as u32;
        let flush_len = size_of::<VirtioGpuResourceFlush>() as u32;
        if st.control.submit_present(xfer_len, flush_len) {
            queue::notify_queue(
                &st.caps.notify_cfg,
                st.caps.notify_off_multiplier,
                &st.control.q,
                st.control.index,
            );
            0
        } else {
            // -1 = not submitted; the compositor keeps the damage pending.
            -1
        }
    }

    /// Park the previous scanout's backing instead of freeing it: consumers
    /// (the vconsole, the mouse bounds, the scanout registry) still point at
    /// it until the caller adopts the new one. Releasing here would leave a
    /// later vconsole blit — a crash restore or a panic screen — writing into
    /// memory the buddy allocator has handed to somebody else.
    fn retire_previous_scanout(&self, old: ScanoutGeom) {
        if old.resource_id == 0 {
            return;
        }
        let _ = self.resource_unref(old.resource_id);
        if old.backing_phys.is_null() {
            return;
        }
        let stale = {
            let mut st = self.state.lock();
            core::mem::replace(&mut st.retired_backing, old.backing_phys)
        };
        if !stale.is_null() {
            free_page_frame(stale);
        }
    }

    fn set_mode(&self, w: u32, h: u32) -> Option<FramebufferData> {
        let (w, h) = sanitize_mode(w, h)?;
        let old = self.state.lock().geom;
        let sl_format = old.sl_format;
        let vfmt = format_from_pixel(sl_format);
        let new_rid = if old.resource_id == RES_FB_PRIMARY {
            RES_FB_ALT
        } else {
            RES_FB_PRIMARY
        };

        let (addr, phys, pitch, size) = self.alloc_backing(w, h)?;

        if !self.resource_create_2d(new_rid, vfmt, w, h)
            || !self.attach_backing(new_rid, phys.as_u64(), size as u32)
            || !self.set_scanout(0, new_rid, w, h)
        {
            let _ = self.resource_unref(new_rid);
            free_page_frame(phys);
            return None;
        }

        let full = VirtioGpuRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let _ = self.transfer_to_host(new_rid, full, 0);
        let _ = self.resource_flush(new_rid, full);

        self.retire_previous_scanout(old);

        {
            let mut st = self.state.lock();
            st.geom = ScanoutGeom {
                ready: true,
                resource_id: new_rid,
                backing_phys: phys,
                width: w,
                height: h,
                pitch,
                sl_format,
            };
        }
        Some(FramebufferData {
            address: addr,
            info: DisplayInfo::new(w, h, pitch, sl_format),
        })
    }

    /// Uploads a 64×64 BGRA cursor image and shows it; lazily creates the
    /// cursor resource and backing on first use.
    fn cursor_set_image(&self, image: &[u8], hot_x: u32, hot_y: u32) -> bool {
        let size = (CURSOR_W * CURSOR_H * 4) as usize;
        let cur = self.state.lock().cursor_state;

        let (phys, addr) = if cur.created {
            let virt = match cur.backing_phys.to_virt_checked() {
                Some(v) => v,
                None => return false,
            };
            (cur.backing_phys, virt.as_u64() as *mut u8)
        } else {
            let pages = size.div_ceil(PAGE_SIZE) as u32;
            let phys = alloc_kernel_pages(pages);
            if phys.is_null() {
                return false;
            }
            let Some(virt) = phys.to_virt_checked() else {
                free_page_frame(phys);
                return false;
            };
            if !self.resource_create_2d(
                RES_CURSOR,
                VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
                CURSOR_W,
                CURSOR_H,
            ) || !self.attach_backing(RES_CURSOR, phys.as_u64(), size as u32)
            {
                let _ = self.resource_unref(RES_CURSOR);
                free_page_frame(phys);
                return false;
            }
            (phys, virt.as_u64() as *mut u8)
        };

        let copy = image.len().min(size);
        if copy > 0 {
            ptr_buf::copy_bytes(addr, image.as_ptr(), copy);
        }
        let full = VirtioGpuRect {
            x: 0,
            y: 0,
            width: CURSOR_W,
            height: CURSOR_H,
        };
        if !self.transfer_to_host(RES_CURSOR, full, 0) {
            return false;
        }

        let (px, py) = {
            let mut st = self.state.lock();
            let (px, py) = (st.cursor_state.x, st.cursor_state.y);
            st.cursor_state = CursorState {
                created: true,
                resource_id: RES_CURSOR,
                backing_phys: phys,
                hot_x,
                hot_y,
                x: px,
                y: py,
            };
            (px, py)
        };

        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_UPDATE_CURSOR),
            pos: VirtioGpuCursorPos {
                scanout_id: 0,
                x: px,
                y: py,
                padding: 0,
            },
            resource_id: RES_CURSOR,
            hot_x,
            hot_y,
            padding: 0,
        };
        self.cursor_cmd(&cmd)
    }

    fn cursor_move(&self, x: u32, y: u32) -> bool {
        let cur = {
            let mut st = self.state.lock();
            if !st.cursor_state.created {
                return false;
            }
            st.cursor_state.x = x;
            st.cursor_state.y = y;
            st.cursor_state
        };
        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr::cmd(VIRTIO_GPU_CMD_MOVE_CURSOR),
            pos: VirtioGpuCursorPos {
                scanout_id: 0,
                x,
                y,
                padding: 0,
            },
            resource_id: cur.resource_id,
            hot_x: cur.hot_x,
            hot_y: cur.hot_y,
            padding: 0,
        };
        self.cursor_cmd(&cmd)
    }

    fn cursor_cmd(&self, cmd: &VirtioGpuUpdateCursor) -> bool {
        let Some(page) = OwnedPageFrame::alloc_zeroed() else {
            return false;
        };
        if !page.write_at(0, cmd) {
            return false;
        }
        self.cursor_submit_page(page, size_of::<VirtioGpuUpdateCursor>() as u32)
    }
}

fn sanitize_mode(w: u32, h: u32) -> Option<(u32, u32)> {
    if w < 320 || h < 240 || w > DisplayInfo::MAX_DIMENSION || h > DisplayInfo::MAX_DIMENSION {
        return None;
    }
    Some((w & !1, h & !1))
}

/// The compositor has already written every damaged pixel into the backing, so
/// transferring the (possibly larger) bounding box is always correct.
fn coalesce_damage(regions: &[DamageRect], w: u32, h: u32) -> Option<VirtioGpuRect> {
    if w == 0 || h == 0 {
        return None;
    }
    let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    let mut any = false;
    for r in regions {
        if !r.is_valid() {
            continue;
        }
        any = true;
        minx = minx.min(r.x0);
        miny = miny.min(r.y0);
        maxx = maxx.max(r.x1);
        maxy = maxy.max(r.y1);
    }
    if !any {
        return None;
    }
    let w1 = w as i32 - 1;
    let h1 = h as i32 - 1;
    let minx = minx.clamp(0, w1);
    let miny = miny.clamp(0, h1);
    let maxx = maxx.clamp(0, w1);
    let maxy = maxy.clamp(0, h1);
    if maxx < minx || maxy < miny {
        return None;
    }
    Some(VirtioGpuRect {
        x: minx as u32,
        y: miny as u32,
        width: (maxx - minx + 1) as u32,
        height: (maxy - miny + 1) as u32,
    })
}

#[cfg(feature = "test-hooks")]
pub mod test_support {
    use super::*;

    pub fn coalesce(regions: &[DamageRect], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        super::coalesce_damage(regions, w, h).map(|r| (r.x, r.y, r.width, r.height))
    }

    pub fn format_code(format: PixelFormat) -> u32 {
        format_from_pixel(format)
    }

    pub fn display_info() -> Option<(u32, u32)> {
        current_device()?.get_display_info()
    }

    /// Control-queue round-trip on a throwaway resource whose id sits far from
    /// the live scanout/cursor ids.
    pub fn resource_roundtrip() -> bool {
        let Some(dev) = current_device() else {
            return false;
        };
        const TEST_RID: u32 = 0x7000;
        let size = (64 * 64 * 4) as usize;
        let pages = size.div_ceil(PAGE_SIZE) as u32;
        let phys = alloc_kernel_pages(pages);
        if phys.is_null() {
            return false;
        }
        let rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        let ok = dev.resource_create_2d(TEST_RID, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, 64, 64)
            && dev.attach_backing(TEST_RID, phys.as_u64(), size as u32)
            && dev.transfer_to_host(TEST_RID, rect, 0)
            && dev.resource_flush(TEST_RID, rect);
        let _ = dev.resource_unref(TEST_RID);
        free_page_frame(phys);
        ok
    }

    /// The image buffer is page-backed rather than stacked: 16 KiB exceeds the
    /// kernel frame limit.
    pub fn cursor_roundtrip() -> bool {
        let Some(dev) = current_device() else {
            return false;
        };
        let size = (CURSOR_W * CURSOR_H * 4) as usize;
        let pages = size.div_ceil(PAGE_SIZE) as u32;
        let phys = alloc_kernel_pages(pages);
        if phys.is_null() {
            return false;
        }
        let Some(virt) = phys.to_virt_checked() else {
            free_page_frame(phys);
            return false;
        };
        let ok = ptr_buf::with_buf::<u8, _>(virt.as_u64() as *const u8, size, |img| {
            dev.cursor_set_image(img, 0, 0)
        }) && dev.cursor_move(10, 10);
        free_page_frame(phys);
        ok
    }

    pub fn sanitize(w: u32, h: u32) -> Option<(u32, u32)> {
        super::sanitize_mode(w, h)
    }
}

static VIRTIO_GPU: SpinLock<Option<KArc<VirtioGpuInner>>> =
    SpinLock::new(None, lock_class!("VIRTIO_GPU", LOCK_LEVEL_REGISTRY));

fn current_device() -> Option<KArc<VirtioGpuInner>> {
    VIRTIO_GPU.lock().clone()
}

pub fn is_present() -> bool {
    VIRTIO_GPU.lock().is_some()
}

/// Flush callback registered with the video framebuffer layer.
pub fn virtio_gpu_flush(damage: *const DamageRect, count: u32) -> c_int {
    match current_device() {
        Some(dev) => dev.present(damage, count),
        None => -1,
    }
}

pub fn set_mode(width: u32, height: u32) -> Option<FramebufferData> {
    current_device()?.set_mode(width, height)
}

pub fn hw_cursor_available() -> bool {
    is_present()
}

/// Upload a 64×64 BGRA hardware cursor image with the given hotspot.
pub fn cursor_set_image(image: &[u8], hot_x: u32, hot_y: u32) -> bool {
    current_device()
        .map(|d| d.cursor_set_image(image, hot_x, hot_y))
        .unwrap_or(false)
}

/// Raw-pointer entry point for the video GPU-control backend, whose fn-pointer
/// table cannot carry a slice lifetime. The pointer/len pair is the kernel-side
/// copy the syscall handler validated.
pub fn cursor_set_image_raw(image: *const u8, len: usize, hot_x: u32, hot_y: u32) -> bool {
    if image.is_null() || len == 0 {
        return false;
    }
    ptr_buf::with_buf::<u8, _>(image, len, |bytes| cursor_set_image(bytes, hot_x, hot_y))
}

/// Move the hardware cursor to absolute display coordinates `(x, y)`.
pub fn cursor_move(x: u32, y: u32) -> bool {
    current_device()
        .map(|d| d.cursor_move(x, y))
        .unwrap_or(false)
}

fn read_num_scanouts(caps: &VirtioMmioCaps) -> u32 {
    if caps.has_device_cfg() {
        caps.device_cfg.read::<u32>(VIRTIO_GPU_CFG_NUM_SCANOUTS)
    } else {
        1
    }
}

/// GPU→GPU re-claim is deferred, so a displaced virtio-gpu has nothing to do.
fn virtio_gpu_evict() {}

fn virtio_gpu_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    // Reserve the scanout before the destructive device reset; a higher-priority
    // owner means staying passive and touching nothing.
    match scanout::SCANOUT.claim(scanout::PRIO_VIRTIO_GPU) {
        ClaimOutcome::Won => {}
        ClaimOutcome::Lost | ClaimOutcome::LostTie => {
            klog_info!("virtio-gpu: lost scanout arbitration; staying passive");
            return Ok(ProbeOutcome::Declined);
        }
    }

    if let Err(err) = virtio_gpu_bring_up(bound) {
        scanout::SCANOUT.abort_claim();
        return Err(err);
    }

    // The blocking GPU commands are safe here: the PCI probe loop runs
    // lock-free with IRQs enabled.
    let Some(gpu_fb) =
        current_device().and_then(|d| d.setup_scanout(scanout::current_framebuffer()))
    else {
        scanout::SCANOUT.abort_claim();
        return Err(PciProbeError::DeviceFault);
    };

    scanout::SCANOUT.commit_install(
        ScanoutProvider {
            id: ScanoutId::VirtioGpu,
            priority: scanout::PRIO_VIRTIO_GPU,
            evict: virtio_gpu_evict,
        },
        scanout::PRIO_VIRTIO_GPU,
        |displaced| {
            if let Some(p) = displaced {
                (p.evict)();
            }
        },
    );

    let ctx = InstallCtx {
        fb: gpu_fb,
        flush: Some(virtio_gpu_flush),
        gpu_control: Some(GpuControlFns {
            available: hw_cursor_available,
            set_image: cursor_set_image_raw,
            move_cursor: cursor_move,
            set_mode,
        }),
    };
    if !scanout::run_scanout_install(&ctx) {
        klog_warn!("virtio-gpu: scanout install failed");
        return Err(PciProbeError::DeviceFault);
    }
    Ok(ProbeOutcome::Bound)
}

/// Initialises the device and publishes it as the live device.
fn virtio_gpu_bring_up(bound: &mut BoundDevice<'_>) -> Result<(), PciProbeError> {
    let info = *bound.info();
    klog_info!(
        "virtio-gpu: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(&info);

    let caps = parse_capabilities(&info);
    if !caps.has_common_cfg() {
        klog_warn!("virtio-gpu: missing common cfg");
        return Err(PciProbeError::Unsupported);
    }

    let feat = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, VIRTIO_GPU_F_EDID);
    if !feat.success {
        klog_warn!("virtio-gpu: feature negotiation failed");
        return Err(PciProbeError::DeviceFault);
    }
    let edid_supported = feat.driver_features & VIRTIO_GPU_F_EDID != 0;

    let inner = match KArc::try_init(VirtioGpuInner::init_empty()) {
        Ok(i) => i,
        Err(_) => return Err(PciProbeError::OutOfMemory),
    };

    let inner_for_irq = inner.clone();
    let (irq_mode, msix_state) = match setup_interrupts(bound, &caps, 2, move |q: u8| {
        inner_for_irq.handle_queue_irq(q)
    }) {
        Ok(v) => v,
        Err(msg) => {
            klog_warn!("virtio-gpu: {} — staying on passive framebuffer", msg);
            return Err(PciProbeError::Unsupported);
        }
    };
    let ctrl_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));
    let cursor_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(1));

    {
        let mut st = inner.state.lock();
        if !queue::setup_queue_into(
            &caps.common_cfg,
            0,
            DEFAULT_QUEUE_SIZE,
            ctrl_entry,
            &mut st.control.q,
        ) {
            klog_warn!("virtio-gpu: control queue setup failed");
            return Err(PciProbeError::OutOfMemory);
        }
        if !queue::setup_queue_into(
            &caps.common_cfg,
            1,
            DEFAULT_QUEUE_SIZE,
            cursor_entry,
            &mut st.cursor.q,
        ) {
            klog_warn!("virtio-gpu: cursor queue setup failed");
            return Err(PciProbeError::OutOfMemory);
        }

        set_driver_ok(&caps);

        st.num_scanouts = read_num_scanouts(&caps);
        st.edid_supported = edid_supported;
        st.caps = caps;
        st.msix_state = msix_state;
        st.ready = true;
    }

    *VIRTIO_GPU.lock() = Some(inner);

    klog_info!(
        "virtio-gpu: ready (edid={}, irq {:?})",
        edid_supported,
        irq_mode
    );
    Ok(())
}

crate::pci_driver! {
    pub static VIRTIO_GPU_DRIVER = {
        name: "virtio-gpu",
        match_table: &[
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_GPU_ID_LEGACY,
            },
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_GPU_ID_MODERN,
            },
        ],
        probe: virtio_gpu_probe,
    };
}
