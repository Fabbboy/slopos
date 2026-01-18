#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{klog_debug, klog_info};

use crate::pci::{PciDeviceInfo, PciDriver, pci_register_driver};
use crate::virtio::{
    self, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VirtioMmioCaps,
    pci::{
        VIRTIO_VENDOR_ID, enable_bus_master, negotiate_features, parse_capabilities, set_driver_ok,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame};

pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: u64 = 512;
const REQUEST_TIMEOUT_SPINS: u32 = 1_000_000;

#[repr(C)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtioBlkDevice {
    bus: u8,
    device: u8,
    function: u8,
    vendor_id: u16,
    device_id: u16,
    queue: Virtqueue,
    capacity_sectors: u64,
    modern: bool,
    ready: bool,
}

impl VirtioBlkDevice {
    const fn new() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            queue: Virtqueue::new(),
            capacity_sectors: 0,
            modern: false,
            ready: false,
        }
    }
}

static DEVICE_CLAIMED: AtomicBool = AtomicBool::new(false);
static mut VIRTIO_BLK_DEVICE: VirtioBlkDevice = VirtioBlkDevice::new();
static mut MMIO_CAPS: VirtioMmioCaps = VirtioMmioCaps::empty();

fn virtio_blk_match(info: *const PciDeviceInfo, _context: *mut c_void) -> bool {
    let info = unsafe { &*info };
    if info.vendor_id != VIRTIO_VENDOR_ID {
        return false;
    }
    info.device_id == VIRTIO_BLK_DEVICE_ID_LEGACY || info.device_id == VIRTIO_BLK_DEVICE_ID_MODERN
}

fn read_capacity(caps: &VirtioMmioCaps) -> u64 {
    if !caps.has_device_cfg() {
        return 0;
    }
    let lo = caps.device_cfg.read_u32(0) as u64;
    let hi = caps.device_cfg.read_u32(4) as u64;
    lo | (hi << 32)
}

fn do_request(
    dev: &mut VirtioBlkDevice,
    caps: &VirtioMmioCaps,
    sector: u64,
    buffer: *mut u8,
    len: usize,
    write: bool,
) -> bool {
    if !dev.queue.is_ready() {
        return false;
    }

    let req_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    if req_page.is_null() {
        return false;
    }

    let bounce_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    if bounce_page.is_null() {
        // TODO: free req_page when we have proper page frame deallocation
        return false;
    }

    let req_virt = req_page.to_virt().as_mut_ptr::<u8>();
    let req_phys = req_page.as_u64();
    let header = req_virt as *mut VirtioBlkReqHeader;
    let status_offset = core::mem::size_of::<VirtioBlkReqHeader>();
    let status_ptr = unsafe { req_virt.add(status_offset) };
    let status_phys = req_phys + status_offset as u64;

    let bounce_virt = bounce_page.to_virt().as_mut_ptr::<u8>();
    let bounce_phys = bounce_page.as_u64();

    if write {
        unsafe {
            core::ptr::copy_nonoverlapping(buffer, bounce_virt, len);
        }
    }

    unsafe {
        (*header).type_ = if write {
            VIRTIO_BLK_T_OUT
        } else {
            VIRTIO_BLK_T_IN
        };
        (*header).reserved = 0;
        (*header).sector = sector;
        *status_ptr = 0xFF;
    }

    dev.queue.write_desc(
        0,
        VirtqDesc {
            addr: req_page.as_u64(),
            len: core::mem::size_of::<VirtioBlkReqHeader>() as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: 1,
        },
    );

    dev.queue.write_desc(
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

    dev.queue.write_desc(
        2,
        VirtqDesc {
            addr: status_phys,
            len: 1,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0,
        },
    );

    dev.queue.submit(0);
    queue::notify_queue(&caps.notify_cfg, caps.notify_off_multiplier, &dev.queue, 0);

    if !dev.queue.poll_used(REQUEST_TIMEOUT_SPINS) {
        klog_info!("virtio-blk: request timeout");
        // TODO: free pages
        return false;
    }

    let status = unsafe { *status_ptr };
    let success = status == VIRTIO_BLK_S_OK;

    if success && !write {
        unsafe {
            core::ptr::copy_nonoverlapping(bounce_virt, buffer, len);
        }
    }

    // TODO: free req_page and bounce_page

    success
}

fn virtio_blk_probe(info: *const PciDeviceInfo, _context: *mut c_void) -> c_int {
    if DEVICE_CLAIMED.swap(true, Ordering::SeqCst) {
        klog_debug!("virtio-blk: already claimed");
        return -1;
    }

    let info = unsafe { &*info };
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
        "virtio-blk: caps common={} notify={} isr={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.isr_cfg.is_mapped(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        DEVICE_CLAIMED.store(false, Ordering::SeqCst);
        return -1;
    }

    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, 0);
    if !feat_result.success {
        klog_info!("virtio-blk: features negotiation failed");
        DEVICE_CLAIMED.store(false, Ordering::SeqCst);
        return -1;
    }

    let queue = match queue::setup_queue(&caps.common_cfg, 0, DEFAULT_QUEUE_SIZE) {
        Some(q) => q,
        None => {
            klog_info!("virtio-blk: queue setup failed");
            DEVICE_CLAIMED.store(false, Ordering::SeqCst);
            return -1;
        }
    };

    set_driver_ok(&caps);

    let capacity_sectors = read_capacity(&caps);
    let is_modern = (feat_result.driver_features & virtio::VIRTIO_F_VERSION_1) != 0;

    let dev = VirtioBlkDevice {
        bus: info.bus,
        device: info.device,
        function: info.function,
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        queue,
        capacity_sectors,
        modern: is_modern,
        ready: true,
    };

    unsafe {
        VIRTIO_BLK_DEVICE = dev;
        MMIO_CAPS = caps;
    }

    klog_info!(
        "virtio-blk: ready, capacity {} sectors ({} MB)",
        capacity_sectors,
        (capacity_sectors * SECTOR_SIZE) / (1024 * 1024)
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
    unsafe { VIRTIO_BLK_DEVICE.ready }
}

pub fn virtio_blk_capacity() -> u64 {
    unsafe { VIRTIO_BLK_DEVICE.capacity_sectors * SECTOR_SIZE }
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
        let ok = unsafe {
            do_request(
                &mut VIRTIO_BLK_DEVICE,
                &MMIO_CAPS,
                sector,
                sector_buf.as_mut_ptr(),
                512,
                false,
            )
        };
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
            let ok = unsafe {
                do_request(
                    &mut VIRTIO_BLK_DEVICE,
                    &MMIO_CAPS,
                    sector,
                    sector_buf.as_mut_ptr(),
                    512,
                    false,
                )
            };
            if !ok {
                return false;
            }
        }

        sector_buf[dst_start..dst_end].copy_from_slice(&buffer[buf_pos..buf_pos + copy_len]);

        let ok = unsafe {
            do_request(
                &mut VIRTIO_BLK_DEVICE,
                &MMIO_CAPS,
                sector,
                sector_buf.as_mut_ptr(),
                512,
                true,
            )
        };
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
