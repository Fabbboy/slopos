#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering, fence};

use slopos_lib::{klog_debug, klog_info};

use crate::pci::{
    PciDeviceInfo, PciDriver, pci_config_read8, pci_config_read16, pci_config_read32,
    pci_config_write16, pci_register_driver,
};
use slopos_abi::arch::x86_64::pci::{
    PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET,
};

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame};

pub const VIRTIO_BLK_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const PCI_STATUS_OFFSET: u8 = 0x06;
const PCI_STATUS_CAP_LIST: u16 = 0x10;
const PCI_CAP_PTR_OFFSET: u8 = 0x34;
const PCI_CAP_ID_VNDR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 0x01;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 0x02;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 0x03;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 0x04;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
const VIRTIO_STATUS_DRIVER: u8 = 0x02;
const VIRTIO_STATUS_FEATURES_OK: u8 = 0x08;
const VIRTIO_STATUS_DRIVER_OK: u8 = 0x04;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTIO_BLK_QUEUE_SIZE: u16 = 64;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: u64 = 512;

#[repr(C)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; VIRTIO_BLK_QUEUE_SIZE as usize],
}

#[repr(C)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; VIRTIO_BLK_QUEUE_SIZE as usize],
}

#[repr(C)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioBlkQueue {
    size: u16,
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
    notify_off: u16,
    last_used_idx: u16,
    ready: u8,
}

impl VirtioBlkQueue {
    const fn new() -> Self {
        Self {
            size: 0,
            desc_phys: 0,
            avail_phys: 0,
            used_phys: 0,
            desc: ptr::null_mut(),
            avail: ptr::null_mut(),
            used: ptr::null_mut(),
            notify_off: 0,
            last_used_idx: 0,
            ready: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct VirtioBlkMmioCaps {
    common_cfg: MmioRegion,
    notify_cfg: MmioRegion,
    notify_off_multiplier: u32,
    isr_cfg: MmioRegion,
    device_cfg: MmioRegion,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtioBlkDevice {
    bus: u8,
    device: u8,
    function: u8,
    vendor_id: u16,
    device_id: u16,
    queue: VirtioBlkQueue,
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
            queue: VirtioBlkQueue::new(),
            capacity_sectors: 0,
            modern: false,
            ready: false,
        }
    }
}

static DEVICE_CLAIMED: AtomicBool = AtomicBool::new(false);
static mut VIRTIO_BLK_DEVICE: VirtioBlkDevice = VirtioBlkDevice::new();
static mut MMIO_CAPS: VirtioBlkMmioCaps = VirtioBlkMmioCaps {
    common_cfg: MmioRegion::empty(),
    notify_cfg: MmioRegion::empty(),
    notify_off_multiplier: 0,
    isr_cfg: MmioRegion::empty(),
    device_cfg: MmioRegion::empty(),
};

fn virtio_blk_match(info: *const PciDeviceInfo, _context: *mut c_void) -> bool {
    let info = unsafe { &*info };
    if info.vendor_id != VIRTIO_BLK_VENDOR_ID {
        return false;
    }
    info.device_id == VIRTIO_BLK_DEVICE_ID_LEGACY || info.device_id == VIRTIO_BLK_DEVICE_ID_MODERN
}

fn enable_bus_master(info: &PciDeviceInfo) {
    let cmd = pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let new_cmd = cmd | PCI_COMMAND_BUS_MASTER | PCI_COMMAND_MEMORY_SPACE;
    pci_config_write16(
        info.bus,
        info.device,
        info.function,
        PCI_COMMAND_OFFSET,
        new_cmd,
    );
}

fn map_cap_region(info: &PciDeviceInfo, bar: u8, offset: u32, length: u32) -> MmioRegion {
    if bar as usize >= info.bars.len() {
        return MmioRegion::empty();
    }
    let bar_info = &info.bars[bar as usize];
    if bar_info.base == 0 || bar_info.is_io != 0 {
        return MmioRegion::empty();
    }
    let phys = PhysAddr::new(bar_info.base + offset as u64);
    MmioRegion::map(phys, length as usize).unwrap_or_else(MmioRegion::empty)
}

fn parse_caps(info: &PciDeviceInfo) -> (VirtioBlkDevice, VirtioBlkMmioCaps) {
    let mut dev = VirtioBlkDevice::new();
    dev.bus = info.bus;
    dev.device = info.device;
    dev.function = info.function;
    dev.vendor_id = info.vendor_id;
    dev.device_id = info.device_id;
    dev.modern = info.device_id == VIRTIO_BLK_DEVICE_ID_MODERN;

    let mut caps = VirtioBlkMmioCaps::default();

    let status = pci_config_read16(info.bus, info.device, info.function, PCI_STATUS_OFFSET);
    if (status & PCI_STATUS_CAP_LIST) == 0 {
        return (dev, caps);
    }

    let mut cap_ptr = pci_config_read8(info.bus, info.device, info.function, PCI_CAP_PTR_OFFSET);
    while cap_ptr != 0 {
        let cap_id = pci_config_read8(info.bus, info.device, info.function, cap_ptr);
        if cap_id == PCI_CAP_ID_VNDR {
            let cfg_type = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 3);
            let bar = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 4);
            let offset = pci_config_read32(info.bus, info.device, info.function, cap_ptr + 8);
            let length = pci_config_read32(info.bus, info.device, info.function, cap_ptr + 12);

            let region = map_cap_region(info, bar, offset, length);

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => caps.common_cfg = region,
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    caps.notify_cfg = region;
                    caps.notify_off_multiplier =
                        pci_config_read32(info.bus, info.device, info.function, cap_ptr + 16);
                }
                VIRTIO_PCI_CAP_ISR_CFG => caps.isr_cfg = region,
                VIRTIO_PCI_CAP_DEVICE_CFG => caps.device_cfg = region,
                _ => {}
            }
        }
        cap_ptr = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 1);
    }

    (dev, caps)
}

fn set_status(cfg: &MmioRegion, status: u8) {
    cfg.write_u8(20, status);
}

fn get_status(cfg: &MmioRegion) -> u8 {
    cfg.read_u8(20)
}

fn negotiate_features(dev: &mut VirtioBlkDevice, caps: &VirtioBlkMmioCaps) -> bool {
    let cfg = &caps.common_cfg;
    if !cfg.is_mapped() {
        return false;
    }

    set_status(cfg, 0);
    let mut status = get_status(cfg);
    status |= VIRTIO_STATUS_ACKNOWLEDGE;
    set_status(cfg, status);

    cfg.write_u32(0, 0);
    let device_features_lo = cfg.read_u32(4);
    cfg.write_u32(0, 1);
    let device_features_hi = cfg.read_u32(4);
    let device_features = (device_features_lo as u64) | ((device_features_hi as u64) << 32);

    let mut driver_features: u64 = 0;
    if (device_features & (1 << 32)) != 0 {
        driver_features |= 1 << 32;
        dev.modern = true;
    }

    cfg.write_u32(8, 0);
    cfg.write_u32(12, driver_features as u32);
    cfg.write_u32(8, 1);
    cfg.write_u32(12, (driver_features >> 32) as u32);

    status |= VIRTIO_STATUS_DRIVER;
    set_status(cfg, status);

    status |= VIRTIO_STATUS_FEATURES_OK;
    set_status(cfg, status);

    let check = get_status(cfg);
    if (check & VIRTIO_STATUS_FEATURES_OK) == 0 {
        klog_info!("virtio-blk: features negotiation failed");
        return false;
    }

    true
}

fn setup_queue(dev: &mut VirtioBlkDevice, caps: &VirtioBlkMmioCaps) -> bool {
    let cfg = &caps.common_cfg;
    if !cfg.is_mapped() {
        return false;
    }

    cfg.write_u16(22, 0);
    let max_size = cfg.read_u16(24);
    if max_size == 0 {
        klog_info!("virtio-blk: queue size 0");
        return false;
    }

    let size = max_size.min(VIRTIO_BLK_QUEUE_SIZE);
    cfg.write_u16(24, size);
    dev.queue.size = size;
    dev.queue.notify_off = cfg.read_u16(30);

    let desc_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    let avail_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    let used_page = alloc_page_frame(ALLOC_FLAG_ZERO);

    if desc_page.is_null() || avail_page.is_null() || used_page.is_null() {
        klog_info!("virtio-blk: queue alloc failed");
        return false;
    }

    dev.queue.desc_phys = desc_page.as_u64();
    dev.queue.avail_phys = avail_page.as_u64();
    dev.queue.used_phys = used_page.as_u64();

    dev.queue.desc = desc_page.to_virt().as_mut_ptr();
    dev.queue.avail = avail_page.to_virt().as_mut_ptr();
    dev.queue.used = used_page.to_virt().as_mut_ptr();

    cfg.write_u64(32, dev.queue.desc_phys);
    cfg.write_u64(40, dev.queue.avail_phys);
    cfg.write_u64(48, dev.queue.used_phys);

    cfg.write_u16(28, 1);
    dev.queue.ready = 1;

    let mut status = get_status(cfg);
    status |= VIRTIO_STATUS_DRIVER_OK;
    set_status(cfg, status);

    true
}

fn read_capacity(dev: &mut VirtioBlkDevice, caps: &VirtioBlkMmioCaps) {
    let cfg = &caps.device_cfg;
    if !cfg.is_mapped() {
        dev.capacity_sectors = 0;
        return;
    }
    let lo = cfg.read_u32(0) as u64;
    let hi = cfg.read_u32(4) as u64;
    dev.capacity_sectors = lo | (hi << 32);
}

fn queue_notify(caps: &VirtioBlkMmioCaps, queue: &VirtioBlkQueue) {
    let offset = (queue.notify_off as u32) * caps.notify_off_multiplier;
    caps.notify_cfg.write_u16(offset as usize, 0);
}

/// Perform a virtio-blk request using a bounce buffer for DMA.
///
/// Virtio requires physical addresses for all descriptors. We allocate a page
/// for the request header + status byte, and another page as a bounce buffer
/// for the actual data. After the request completes, we copy data to/from the
/// caller's buffer.
fn do_request(
    dev: &mut VirtioBlkDevice,
    caps: &VirtioBlkMmioCaps,
    sector: u64,
    buffer: *mut u8,
    len: usize,
    write: bool,
) -> bool {
    if dev.queue.ready == 0 {
        return false;
    }

    // Allocate a page for request header + status byte
    let req_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    if req_page.is_null() {
        return false;
    }

    // Allocate a bounce buffer page for DMA (virtio needs physical addresses)
    let bounce_page = alloc_page_frame(ALLOC_FLAG_ZERO);
    if bounce_page.is_null() {
        // TODO: free req_page
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

    // If writing, copy data from caller's buffer to bounce buffer
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

    let desc = dev.queue.desc;
    let avail = dev.queue.avail;
    let used = dev.queue.used;

    unsafe {
        // Descriptor 0: request header (device reads)
        (*desc.add(0)).addr = req_page.as_u64();
        (*desc.add(0)).len = core::mem::size_of::<VirtioBlkReqHeader>() as u32;
        (*desc.add(0)).flags = VIRTQ_DESC_F_NEXT;
        (*desc.add(0)).next = 1;

        // Descriptor 1: data buffer (physical address of bounce buffer)
        (*desc.add(1)).addr = bounce_phys;
        (*desc.add(1)).len = len as u32;
        (*desc.add(1)).flags = if write {
            VIRTQ_DESC_F_NEXT
        } else {
            VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
        };
        (*desc.add(1)).next = 2;

        // Descriptor 2: status byte (device writes, physical address)
        (*desc.add(2)).addr = status_phys;
        (*desc.add(2)).len = 1;
        (*desc.add(2)).flags = VIRTQ_DESC_F_WRITE;
        (*desc.add(2)).next = 0;

        let avail_idx = (*avail).idx;
        (*avail).ring[(avail_idx % dev.queue.size) as usize] = 0;
        fence(Ordering::SeqCst);
        (*avail).idx = avail_idx.wrapping_add(1);
        fence(Ordering::SeqCst);
    }

    queue_notify(caps, &dev.queue);

    let mut timeout = 1_000_000u32;
    loop {
        fence(Ordering::SeqCst);
        let used_idx = unsafe { (*used).idx };
        if used_idx != dev.queue.last_used_idx {
            dev.queue.last_used_idx = used_idx;
            break;
        }
        timeout = timeout.saturating_sub(1);
        if timeout == 0 {
            klog_info!("virtio-blk: request timeout");
            // TODO: free req_page and bounce_page
            return false;
        }
        core::hint::spin_loop();
    }

    let status = unsafe { *status_ptr };
    let success = status == VIRTIO_BLK_S_OK;

    // If reading successfully, copy data from bounce buffer to caller's buffer
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

    let (mut dev, caps) = parse_caps(info);

    klog_debug!(
        "virtio-blk: caps common={} notify={} isr={} device={}",
        caps.common_cfg.is_mapped(),
        caps.notify_cfg.is_mapped(),
        caps.isr_cfg.is_mapped(),
        caps.device_cfg.is_mapped()
    );

    if !caps.common_cfg.is_mapped() {
        klog_info!("virtio-blk: missing common cfg");
        DEVICE_CLAIMED.store(false, Ordering::SeqCst);
        return -1;
    }

    if !negotiate_features(&mut dev, &caps) {
        DEVICE_CLAIMED.store(false, Ordering::SeqCst);
        return -1;
    }

    if !setup_queue(&mut dev, &caps) {
        DEVICE_CLAIMED.store(false, Ordering::SeqCst);
        return -1;
    }

    read_capacity(&mut dev, &caps);
    dev.ready = true;

    unsafe {
        VIRTIO_BLK_DEVICE = dev;
        MMIO_CAPS = caps;
    }

    klog_info!(
        "virtio-blk: ready, capacity {} sectors ({} MB)",
        dev.capacity_sectors,
        (dev.capacity_sectors * SECTOR_SIZE) / (1024 * 1024)
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
