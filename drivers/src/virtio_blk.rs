use core::ffi::c_int;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::dev::FromRawPtr;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_utils::{klog_debug, klog_info};

use crate::pci::{PciDeviceInfo, PciDriver, pci_register_driver};
use crate::virtio::{
    self, CompletionEvent, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
    VirtioMmioCaps, VirtioMsixState,
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
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: u64 = 512;
const REQUEST_TIMEOUT_MS: u32 = 5000;

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
}

impl VirtioBlkDevice {
    const fn new() -> Self {
        Self {
            queue: Virtqueue::new(),
            capacity_sectors: 0,
            ready: false,
        }
    }
}

/// Combined device + MMIO caps + interrupt state under a single lock,
/// ensuring ownership/claim state and the request path share one coherent
/// synchronization model.
struct VirtioBlkState {
    device: VirtioBlkDevice,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
}

impl VirtioBlkState {
    const fn new() -> Self {
        Self {
            device: VirtioBlkDevice::new(),
            caps: VirtioMmioCaps::empty(),
            msix_state: None,
        }
    }
}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static VIRTIO_BLK_STATE: SpinLock<VirtioBlkState> =
    SpinLock::new(VirtioBlkState::new(), LOCK_LEVEL_RESOURCE);
static BLK_QUEUE_EVENT: CompletionEvent = CompletionEvent::new();
static BLK_REQUEST_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct RequestGuard;

impl RequestGuard {
    fn acquire(flag: &AtomicBool) -> Self {
        while flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Self
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        BLK_REQUEST_IN_FLIGHT.store(false, Ordering::Release);
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

fn virtio_blk_match(info: *const PciDeviceInfo, _context: *mut core::ffi::c_void) -> bool {
    let Some(info) = PciDeviceInfo::from_ptr(info) else {
        return false;
    };
    if info.vendor_id != PCI_VENDOR_ID_VIRTIO {
        return false;
    }
    info.device_id == VIRTIO_BLK_DEVICE_ID_LEGACY || info.device_id == VIRTIO_BLK_DEVICE_ID_MODERN
}

fn read_capacity(caps: &VirtioMmioCaps) -> u64 {
    if !caps.has_device_cfg() {
        return 0;
    }
    let lo = caps.device_cfg.read::<u32>(0) as u64;
    let hi = caps.device_cfg.read::<u32>(4) as u64;
    lo | (hi << 32)
}

fn do_request(sector: u64, buffer: &mut [u8], write: bool) -> bool {
    let _request_guard = RequestGuard::acquire(&BLK_REQUEST_IN_FLIGHT);

    {
        let state = VIRTIO_BLK_STATE.lock();
        if !state.device.queue.is_ready() {
            return false;
        }
    }

    let buffers = match RequestBuffers::allocate() {
        Some(b) => b,
        None => return false,
    };

    let req_phys = buffers.req_page.phys_u64();
    let status_offset = size_of::<VirtioBlkReqHeader>();
    let status_phys = req_phys + status_offset as u64;
    let bounce_phys = buffers.bounce_page.phys_u64();
    let len = buffer.len();

    if write && !buffers.bounce_page.write_slice(0, buffer) {
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
    if !buffers.req_page.write_at::<VirtioBlkReqHeader>(0, &header) {
        return false;
    }
    if !buffers
        .req_page
        .write_volatile_at::<u8>(status_offset, 0xFF)
    {
        return false;
    }

    {
        let mut state = VIRTIO_BLK_STATE.lock();
        BLK_QUEUE_EVENT.reset();

        state.device.queue.write_desc(
            0,
            VirtqDesc {
                addr: req_phys,
                len: size_of::<VirtioBlkReqHeader>() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: 1,
            },
        );

        state.device.queue.write_desc(
            1,
            VirtqDesc {
                addr: bounce_phys,
                len: len as u32,
                flags: if write {
                    VIRTQ_DESC_F_NEXT
                } else {
                    VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
                },
                next: 2,
            },
        );

        state.device.queue.write_desc(
            2,
            VirtqDesc {
                addr: status_phys,
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );

        state.device.queue.submit(0);
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.device.queue,
            0,
        );
    }

    if !BLK_QUEUE_EVENT.wait_timeout_ms(REQUEST_TIMEOUT_MS) {
        klog_info!("virtio-blk: request timeout");
        return false;
    }

    {
        let mut state = VIRTIO_BLK_STATE.lock();
        if !state.device.queue.advance_used() {
            klog_info!("virtio-blk: signaled without used completion");
            return false;
        }
    }

    let status = buffers
        .req_page
        .read_volatile_at::<u8>(status_offset)
        .unwrap_or(0xFF);
    let success = status == VIRTIO_BLK_S_OK;

    if success && !write && !buffers.bounce_page.read_slice(0, buffer) {
        return false;
    }

    success
}

/// Per-queue interrupt handler for virtio-blk.
///
/// Invoked by the OSTD IRQ dispatch layer when the device fires an
/// MSI-X (per-queue) or MSI (shared) vector. The handler signals the
/// queue completion event consumed by [`do_request`].
fn virtio_blk_irq_handler(_queue_idx: u8) {
    BLK_QUEUE_EVENT.signal();
}

fn virtio_blk_probe(info: *const PciDeviceInfo, _context: *mut core::ffi::c_void) -> c_int {
    if !DEVICE_CLAIMED.claim() {
        klog_debug!("virtio-blk: already claimed");
        return -1;
    }

    let Some(info) = PciDeviceInfo::from_ptr(info) else {
        DEVICE_CLAIMED.reset();
        return -1;
    };
    klog_info!(
        "virtio-blk: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(info);

    let caps = parse_capabilities(info);

    klog_debug!(
        "virtio-blk: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        DEVICE_CLAIMED.reset();
        return -1;
    }

    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, 0);
    if !feat_result.success {
        klog_info!("virtio-blk: features negotiation failed");
        DEVICE_CLAIMED.reset();
        return -1;
    }

    // --- MSI-X / MSI interrupt setup ---
    // VirtIO modern on q35 always has MSI-X; MSI is the minimum fallback.
    // setup_interrupts allocates IDT vectors via OSTD's IrqAllocator, registers
    // a closure that calls virtio_blk_irq_handler with the queue index, and
    // programs the device's MSI-X/MSI capability.
    let (irq_mode, msix_state) = setup_interrupts(info, &caps, 1, virtio_blk_irq_handler)
        .unwrap_or_else(|msg| {
            panic!(
                "virtio-blk: {}:{}.{} {}",
                info.bus, info.device, info.function, msg
            )
        });
    let q0_msix_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));

    let queue = match queue::setup_queue(&caps.common_cfg, 0, DEFAULT_QUEUE_SIZE, q0_msix_entry) {
        Some(q) => q,
        None => {
            klog_info!("virtio-blk: queue setup failed");
            DEVICE_CLAIMED.reset();
            return -1;
        }
    };

    set_driver_ok(&caps);

    let capacity_sectors = read_capacity(&caps);

    let dev = VirtioBlkDevice {
        queue,
        capacity_sectors,
        ready: true,
    };

    {
        let mut state = VIRTIO_BLK_STATE.lock();
        state.device = dev;
        state.caps = caps;
        state.msix_state = msix_state;
    }

    klog_info!(
        "virtio-blk: ready, capacity {} sectors ({} MB), irq {:?}",
        capacity_sectors,
        (capacity_sectors * SECTOR_SIZE) / (1024 * 1024),
        irq_mode,
    );

    0
}
static VIRTIO_BLK_DRIVER: PciDriver = PciDriver {
    name: b"virtio-blk\0".as_ptr(),
    match_fn: Some(virtio_blk_match),
    probe: Some(virtio_blk_probe),
    context: ptr::null_mut(),
};

pub fn virtio_blk_register_driver() {
    if pci_register_driver(&VIRTIO_BLK_DRIVER) != 0 {
        klog_info!("virtio-blk: driver registration failed");
    }
}

pub fn virtio_blk_is_ready() -> bool {
    VIRTIO_BLK_STATE.lock().device.ready
}

pub fn virtio_blk_capacity() -> u64 {
    let state = VIRTIO_BLK_STATE.lock();
    state.device.capacity_sectors * SECTOR_SIZE
}

pub fn virtio_blk_read(offset: u64, buffer: &mut [u8]) -> bool {
    if buffer.is_empty() {
        return true;
    }

    if !virtio_blk_is_ready() {
        return false;
    }

    let start_sector = offset / SECTOR_SIZE;
    let sector_offset = (offset % SECTOR_SIZE) as usize;

    let mut sector_buf = [0u8; 512];
    let sectors_needed = (sector_offset + buffer.len() + 511) / 512;

    let mut buf_pos = 0usize;
    for i in 0..sectors_needed {
        let sector = start_sector + i as u64;
        let ok = do_request(sector, &mut sector_buf, false);
        if !ok {
            return false;
        }

        let src_start = if i == 0 { sector_offset } else { 0 };
        let src_end = 512.min(src_start + (buffer.len() - buf_pos));
        let copy_len = src_end - src_start;

        buffer[buf_pos..buf_pos + copy_len].copy_from_slice(&sector_buf[src_start..src_end]);
        buf_pos += copy_len;

        if buf_pos >= buffer.len() {
            break;
        }
    }

    true
}

pub fn virtio_blk_write(offset: u64, buffer: &[u8]) -> bool {
    if buffer.is_empty() {
        return true;
    }

    if !virtio_blk_is_ready() {
        return false;
    }

    let start_sector = offset / SECTOR_SIZE;
    let sector_offset = (offset % SECTOR_SIZE) as usize;

    let mut sector_buf = [0u8; 512];
    let sectors_needed = (sector_offset + buffer.len() + 511) / 512;

    let mut buf_pos = 0usize;
    for i in 0..sectors_needed {
        let sector = start_sector + i as u64;

        let dst_start = if i == 0 { sector_offset } else { 0 };
        let dst_end = 512.min(dst_start + (buffer.len() - buf_pos));
        let copy_len = dst_end - dst_start;

        if dst_start != 0 || dst_end != 512 {
            let ok = do_request(sector, &mut sector_buf, false);
            if !ok {
                return false;
            }
        }

        sector_buf[dst_start..dst_end].copy_from_slice(&buffer[buf_pos..buf_pos + copy_len]);

        let ok = do_request(sector, &mut sector_buf, true);
        if !ok {
            return false;
        }

        buf_pos += copy_len;
        if buf_pos >= buffer.len() {
            break;
        }
    }

    true
}

// =============================================================================
// Test-only accessors
// =============================================================================

/// Return a snapshot of the MSI-X state for the claimed VirtIO-blk device.
///
/// Only available in test builds (`test-hooks` feature).  Returns `None` if the
/// device was not probed or MSI-X was not configured (i.e. MSI fallback).
#[cfg(feature = "test-hooks")]
pub fn virtio_blk_msix_state() -> Option<VirtioMsixState> {
    VIRTIO_BLK_STATE.lock().msix_state.clone()
}
