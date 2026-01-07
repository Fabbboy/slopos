#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{FramebufferInfo, align_up, klog_debug, klog_info};

use crate::hw::pci_defs::{PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET};
use crate::pci::{
    PciBarInfo, PciDeviceInfo, PciDriver, pci_config_read8, pci_config_read16,
    pci_config_read32, pci_config_write8, pci_config_write16, pci_register_driver,
};
use crate::wl_currency;

use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, alloc_page_frames, free_page_frame};
use slopos_mm::phys_virt::{mm_map_mmio_region, mm_phys_to_virt, mm_unmap_mmio_region};

pub const VIRTIO_GPU_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_GPU_DEVICE_ID_PRIMARY: u16 = 0x1050;
pub const VIRTIO_GPU_DEVICE_ID_TRANS: u16 = 0x1010;

const VIRTIO_PCI_STATUS_OFFSET: u8 = 0x12;
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
const VIRTIO_STATUS_DRIVER: u8 = 0x02;
const VIRTIO_STATUS_FEATURES_OK: u8 = 0x08;
const VIRTIO_STATUS_DRIVER_OK: u8 = 0x04;

const PCI_STATUS_OFFSET: u8 = 0x06;
const PCI_STATUS_CAP_LIST: u16 = 0x10;
const PCI_CAP_PTR_OFFSET: u8 = 0x34;
const PCI_CAP_ID_VNDR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 0x01;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 0x02;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 0x03;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 0x04;

const VIRTIO_F_VERSION_1: u32 = 1 << 0;
const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0;

const VIRTIO_MMIO_DEFAULT_SIZE: usize = 0x1000;
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTIO_GPU_QUEUE_CONTROL: u16 = 0;
const VIRTIO_GPU_QUEUE_SIZE: u16 = 64;
use slopos_mm::mm_constants::PAGE_SIZE_4KB;

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;

#[repr(C)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    msix_config: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_avail: u64,
    queue_used: u64,
}

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
    ring: [u16; VIRTIO_GPU_QUEUE_SIZE as usize],
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
    ring: [VirtqUsedElem; VIRTIO_GPU_QUEUE_SIZE as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioGpuQueue {
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

impl VirtioGpuQueue {
    const fn new() -> Self {
        Self {
            size: 0,
            desc_phys: 0,
            avail_phys: 0,
            used_phys: 0,
            desc: core::ptr::null_mut(),
            avail: core::ptr::null_mut(),
            used: core::ptr::null_mut(),
            notify_off: 0,
            last_used_idx: 0,
            ready: 0,
        }
    }
}

#[repr(C)]
struct VirtioGpuCtrlHeader {
    type_: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioGpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioGpuDisplayOne {
    rect: VirtioGpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
struct VirtioGpuRespDisplayInfo {
    header: VirtioGpuCtrlHeader,
    displays: [VirtioGpuDisplayOne; 16],
}

#[repr(C)]
struct VirtioGpuGetCapsetInfo {
    header: VirtioGpuCtrlHeader,
    capset_index: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuRespCapsetInfo {
    header: VirtioGpuCtrlHeader,
    capset_id: u32,
    capset_max_version: u32,
    capset_max_size: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuResourceCreate2d {
    header: VirtioGpuCtrlHeader,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct VirtioGpuResourceAttachBacking {
    header: VirtioGpuCtrlHeader,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuSetScanout {
    header: VirtioGpuCtrlHeader,
    rect: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
struct VirtioGpuTransferToHost2d {
    header: VirtioGpuCtrlHeader,
    rect: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuResourceFlush {
    header: VirtioGpuCtrlHeader,
    rect: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuCtxCreate {
    header: VirtioGpuCtrlHeader,
    ctx_id: u32,
    name_len: u32,
    padding0: u32,
    padding1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct virtio_gpu_device_t {
    pub present: c_int,
    pub device: PciDeviceInfo,
    pub mmio_base: *mut core::ffi::c_void,
    pub mmio_size: usize,
    common_cfg: *mut VirtioPciCommonCfg,
    pub notify_cfg: *mut u8,
    pub notify_off_multiplier: u32,
    pub isr_cfg: *mut u8,
    pub device_cfg: *mut u8,
    pub device_cfg_len: u32,
    pub supports_virgl: u8,
    pub modern_caps: u8,
    ctrl_queue: VirtioGpuQueue,
    pub virgl_ready: u8,
    pub display_width: u32,
    pub display_height: u32,
    pub fb_resource_id: u32,
    pub fb_phys: u64,
    pub fb_size: u64,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_pitch: u32,
    pub fb_bpp: u16,
    pub fb_ready: u8,
}

static mut VIRTIO_GPU_DEVICE: virtio_gpu_device_t = virtio_gpu_device_t {
    present: 0,
    device: PciDeviceInfo::zeroed(),
    mmio_base: core::ptr::null_mut(),
    mmio_size: 0,
    common_cfg: core::ptr::null_mut(),
    notify_cfg: core::ptr::null_mut(),
    notify_off_multiplier: 0,
    isr_cfg: core::ptr::null_mut(),
    device_cfg: core::ptr::null_mut(),
    device_cfg_len: 0,
    supports_virgl: 0,
    modern_caps: 0,
    ctrl_queue: VirtioGpuQueue::new(),
    virgl_ready: 0,
    display_width: 0,
    display_height: 0,
    fb_resource_id: 0,
    fb_phys: 0,
    fb_size: 0,
    fb_width: 0,
    fb_height: 0,
    fb_pitch: 0,
    fb_bpp: 0,
    fb_ready: 0,
};

fn virtio_gpu_enable_master(info: &PciDeviceInfo) {
    let command = pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let desired = command | PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER;
    if command != desired {
        pci_config_write16(
            info.bus,
            info.device,
            info.function,
            PCI_COMMAND_OFFSET,
            desired,
        );
    }
}

fn virtio_gpu_match(info: *const PciDeviceInfo, _context: *mut c_void) -> bool {
    let info = unsafe { &*info };
    if info.vendor_id != VIRTIO_GPU_VENDOR_ID {
        return false;
    }
    info.device_id == VIRTIO_GPU_DEVICE_ID_PRIMARY || info.device_id == VIRTIO_GPU_DEVICE_ID_TRANS
}

fn virtio_gpu_map_cap_region(
    info: &PciDeviceInfo,
    bar_index: u8,
    offset: u32,
    length: u32,
) -> *mut u8 {
    let bar = info.bars.get(bar_index as usize);
    let bar = match bar {
        Some(b) => b,
        None => return core::ptr::null_mut(),
    };
    if bar.is_io != 0 || bar.base == 0 || length == 0 {
        return core::ptr::null_mut();
    }
    let phys = bar.base.wrapping_add(offset as u64);
    mm_map_mmio_region(phys, length as usize) as *mut u8
}

fn virtio_gpu_parse_caps(info: &PciDeviceInfo) -> virtio_gpu_device_t {
    let mut caps = virtio_gpu_device_t::default();

    let status = pci_config_read16(info.bus, info.device, info.function, PCI_STATUS_OFFSET);
    if (status & PCI_STATUS_CAP_LIST) == 0 {
        return caps;
    }

    let mut cap_ptr = pci_config_read8(info.bus, info.device, info.function, PCI_CAP_PTR_OFFSET);
    let mut guard = 0;
    while cap_ptr != 0 && guard < 48 {
        guard += 1;
        let cap_id = pci_config_read8(info.bus, info.device, info.function, cap_ptr);
        let cap_next = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 1);
        let cap_len = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 2);
        if cap_id == PCI_CAP_ID_VNDR && cap_len >= 16 {
            let cfg_type = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 3);
            let bar = pci_config_read8(info.bus, info.device, info.function, cap_ptr + 4);
            let offset = pci_config_read32(info.bus, info.device, info.function, cap_ptr + 8);
            let length = pci_config_read32(info.bus, info.device, info.function, cap_ptr + 12);
            let mapped = virtio_gpu_map_cap_region(info, bar, offset, length);
            if !mapped.is_null() {
                match cfg_type {
                    VIRTIO_PCI_CAP_COMMON_CFG => {
                        caps.common_cfg = mapped as *mut VirtioPciCommonCfg;
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                        caps.notify_cfg = mapped;
                        caps.notify_off_multiplier =
                            pci_config_read32(info.bus, info.device, info.function, cap_ptr + 16);
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_ISR_CFG => {
                        caps.isr_cfg = mapped;
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_DEVICE_CFG => {
                        caps.device_cfg = mapped;
                        caps.device_cfg_len = length;
                        caps.modern_caps = 1;
                    }
                    _ => {}
                }
            }
        }
        cap_ptr = cap_next;
    }

    caps
}

fn virtio_gpu_set_status(cfg: *mut VirtioPciCommonCfg, status: u8) {
    unsafe {
        let mut current = core::ptr::read_volatile(cfg);
        current.device_status = status;
        core::ptr::write_volatile(cfg, current);
    }
}

fn virtio_gpu_get_status(cfg: *mut VirtioPciCommonCfg) -> u8 {
    unsafe { core::ptr::read_volatile(&(*cfg).device_status) }
}

fn virtio_gpu_negotiate_features(device: &mut virtio_gpu_device_t) -> bool {
    let cfg = device.common_cfg;
    if cfg.is_null() {
        return false;
    }

    virtio_gpu_set_status(cfg, 0);
    let mut status = virtio_gpu_get_status(cfg);
    status |= VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
    virtio_gpu_set_status(cfg, status);

    unsafe {
        core::ptr::write_volatile(&mut (*cfg).device_feature_select, 0);
    }
    let features_low = unsafe { core::ptr::read_volatile(&(*cfg).device_feature) };

    unsafe {
        core::ptr::write_volatile(&mut (*cfg).device_feature_select, 1);
    }
    let features_high = unsafe { core::ptr::read_volatile(&(*cfg).device_feature) };

    let driver_features_low = features_low & VIRTIO_GPU_F_VIRGL;
    let driver_features_high = features_high & VIRTIO_F_VERSION_1;

    unsafe {
        core::ptr::write_volatile(&mut (*cfg).driver_feature_select, 0);
        core::ptr::write_volatile(&mut (*cfg).driver_feature, driver_features_low);
        core::ptr::write_volatile(&mut (*cfg).driver_feature_select, 1);
        core::ptr::write_volatile(&mut (*cfg).driver_feature, driver_features_high);
    }

    status |= VIRTIO_STATUS_FEATURES_OK;
    virtio_gpu_set_status(cfg, status);
    status = virtio_gpu_get_status(cfg);
    if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
        return false;
    }

    if (features_low & VIRTIO_GPU_F_VIRGL) != 0 {
        device.supports_virgl = 1;
    }

    true
}

fn virtio_gpu_setup_control_queue(device: &mut virtio_gpu_device_t) -> bool {
    let cfg = device.common_cfg;
    if cfg.is_null() || device.notify_cfg.is_null() {
        return false;
    }

    unsafe {
        core::ptr::write_volatile(&mut (*cfg).queue_select, VIRTIO_GPU_QUEUE_CONTROL);
    }
    let queue_size = unsafe { core::ptr::read_volatile(&(*cfg).queue_size) };
    if queue_size == 0 {
        return false;
    }
    let size = core::cmp::min(queue_size, VIRTIO_GPU_QUEUE_SIZE);

    let desc_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let avail_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let used_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if desc_phys == 0 || avail_phys == 0 || used_phys == 0 {
        if desc_phys != 0 {
            free_page_frame(desc_phys);
        }
        if avail_phys != 0 {
            free_page_frame(avail_phys);
        }
        if used_phys != 0 {
            free_page_frame(used_phys);
        }
        return false;
    }

    let desc_virt = mm_phys_to_virt(desc_phys);
    let avail_virt = mm_phys_to_virt(avail_phys);
    let used_virt = mm_phys_to_virt(used_phys);
    if desc_virt == 0 || avail_virt == 0 || used_virt == 0 {
        free_page_frame(desc_phys);
        free_page_frame(avail_phys);
        free_page_frame(used_phys);
        return false;
    }

    unsafe {
        core::ptr::write_volatile(&mut (*cfg).queue_desc, desc_phys);
        core::ptr::write_volatile(&mut (*cfg).queue_avail, avail_phys);
        core::ptr::write_volatile(&mut (*cfg).queue_used, used_phys);
        core::ptr::write_volatile(&mut (*cfg).queue_enable, 1);
    }

    let notify_off = unsafe { core::ptr::read_volatile(&(*cfg).queue_notify_off) };

    device.ctrl_queue = VirtioGpuQueue {
        size,
        desc_phys,
        avail_phys,
        used_phys,
        desc: desc_virt as *mut VirtqDesc,
        avail: avail_virt as *mut VirtqAvail,
        used: used_virt as *mut VirtqUsed,
        notify_off,
        last_used_idx: 0,
        ready: 1,
    };

    let mut status = virtio_gpu_get_status(cfg);
    status |= VIRTIO_STATUS_DRIVER_OK;
    virtio_gpu_set_status(cfg, status);

    true
}

fn virtio_gpu_queue_notify(notify_cfg: *mut u8, notify_off_multiplier: u32, queue: &VirtioGpuQueue) {
    if notify_cfg.is_null() {
        return;
    }
    let offset = (queue.notify_off as u32 * notify_off_multiplier) as usize;
    unsafe {
        let notify = notify_cfg.add(offset) as *mut u16;
        core::ptr::write_volatile(notify, VIRTIO_GPU_QUEUE_CONTROL);
    }
}

fn virtio_gpu_send_cmd(
    queue: &mut VirtioGpuQueue,
    notify_cfg: *mut u8,
    notify_off_multiplier: u32,
    cmd_phys: u64,
    cmd_len: usize,
    resp_phys: u64,
    resp_len: usize,
) -> bool {
    let desc = queue.desc;
    if desc.is_null() {
        return false;
    }

    unsafe {
        core::ptr::write_volatile(
            &mut *desc,
            VirtqDesc {
                addr: cmd_phys,
                len: cmd_len as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: 1,
            },
        );
        core::ptr::write_volatile(
            desc.add(1),
            VirtqDesc {
                addr: resp_phys,
                len: resp_len as u32,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
    }

    virtio_gpu_queue_submit(queue, notify_cfg, notify_off_multiplier, 0)
}

fn virtio_gpu_queue_submit(
    queue: &mut VirtioGpuQueue,
    notify_cfg: *mut u8,
    notify_off_multiplier: u32,
    head: u16,
) -> bool {
    if queue.ready == 0 {
        return false;
    }
    let avail = queue.avail;
    let used = queue.used;
    if avail.is_null() || used.is_null() {
        return false;
    }

    unsafe {
        let idx = core::ptr::read_volatile(&(*avail).idx);
        let ring_index = (idx % queue.size) as usize;
        core::ptr::write_volatile(&mut (*avail).ring[ring_index], head);
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        core::ptr::write_volatile(&mut (*avail).idx, idx.wrapping_add(1));
    }

    virtio_gpu_queue_notify(notify_cfg, notify_off_multiplier, queue);

    let mut spins = 0u32;
    loop {
        let used_idx = unsafe { core::ptr::read_volatile(&(*used).idx) };
        if used_idx != queue.last_used_idx {
            queue.last_used_idx = used_idx;
            return true;
        }
        spins += 1;
        if spins > 1_000_000 {
            break;
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
    }
    false
}

fn virtio_gpu_get_display_info(device: &mut virtio_gpu_device_t) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuCtrlHeader;
    let resp = resp_virt as *mut VirtioGpuRespDisplayInfo;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuCtrlHeader {
                type_: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
        );
    }
    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuRespDisplayInfo>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(&(*resp).header) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let display = unsafe { core::ptr::read_volatile(&(*resp).displays[0]) };
    if display.enabled != 0 {
        device.display_width = display.rect.width;
        device.display_height = display.rect.height;
        klog_info!(
            "PCI: virtio-gpu display0 {}x{} @ ({},{})",
            display.rect.width,
            display.rect.height,
            display.rect.x,
            display.rect.y
        );
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_get_capset_info(device: &mut virtio_gpu_device_t) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuGetCapsetInfo;
    let resp = resp_virt as *mut VirtioGpuRespCapsetInfo;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuGetCapsetInfo {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_GET_CAPSET_INFO,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                capset_index: 0,
                padding: 0,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuGetCapsetInfo>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuRespCapsetInfo>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(&(*resp).header) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_CAPSET_INFO {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let capset_id = unsafe { core::ptr::read_volatile(&(*resp).capset_id) };
    let capset_version = unsafe { core::ptr::read_volatile(&(*resp).capset_max_version) };
    let capset_size = unsafe { core::ptr::read_volatile(&(*resp).capset_max_size) };
    klog_info!(
        "PCI: virtio-gpu capset id {} (max ver {}, max size {})",
        capset_id,
        capset_version,
        capset_size
    );
    if capset_id == VIRTIO_GPU_CAPSET_VIRGL || capset_id == VIRTIO_GPU_CAPSET_VIRGL2 {
        klog_info!("PCI: virtio-gpu virgl capset detected");
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_create_context(device: &mut virtio_gpu_device_t, ctx_id: u32) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuCtxCreate;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuCtxCreate {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_CTX_CREATE,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                ctx_id,
                name_len: 0,
                padding0: 0,
                padding1: 0,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuCtxCreate>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_resource_create_2d(
    device: &mut virtio_gpu_device_t,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuResourceCreate2d;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuResourceCreate2d {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                resource_id,
                format: VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
                width,
                height,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuResourceCreate2d>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_resource_attach_backing(
    device: &mut virtio_gpu_device_t,
    resource_id: u32,
    backing_phys: u64,
    backing_len: u32,
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuResourceAttachBacking;
    let entry = unsafe { (cmd as *mut u8).add(core::mem::size_of::<VirtioGpuResourceAttachBacking>()) }
        as *mut VirtioGpuMemEntry;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuResourceAttachBacking {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                resource_id,
                nr_entries: 1,
            },
        );
        core::ptr::write_volatile(
            entry,
            VirtioGpuMemEntry {
                addr: backing_phys,
                length: backing_len,
                padding: 0,
            },
        );
    }

    let cmd_len =
        core::mem::size_of::<VirtioGpuResourceAttachBacking>() + core::mem::size_of::<VirtioGpuMemEntry>();
    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        cmd_len,
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_set_scanout(
    device: &mut virtio_gpu_device_t,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuSetScanout;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuSetScanout {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_SET_SCANOUT,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                rect: VirtioGpuRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                scanout_id: 0,
                resource_id,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuSetScanout>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_transfer_to_host_2d(
    device: &mut virtio_gpu_device_t,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuTransferToHost2d;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuTransferToHost2d {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                rect: VirtioGpuRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                offset: 0,
                resource_id,
                padding: 0,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuTransferToHost2d>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_resource_flush(
    device: &mut virtio_gpu_device_t,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if cmd_phys == 0 || resp_phys == 0 {
        if cmd_phys != 0 {
            free_page_frame(cmd_phys);
        }
        if resp_phys != 0 {
            free_page_frame(resp_phys);
        }
        return false;
    }

    let cmd_virt = mm_phys_to_virt(cmd_phys);
    let resp_virt = mm_phys_to_virt(resp_phys);
    if cmd_virt == 0 || resp_virt == 0 {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let cmd = cmd_virt as *mut VirtioGpuResourceFlush;
    let resp = resp_virt as *mut VirtioGpuCtrlHeader;
    unsafe {
        core::ptr::write_volatile(
            cmd,
            VirtioGpuResourceFlush {
                header: VirtioGpuCtrlHeader {
                    type_: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                rect: VirtioGpuRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                resource_id,
                padding: 0,
            },
        );
    }

    let notify_cfg = device.notify_cfg;
    let notify_mult = device.notify_off_multiplier;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        notify_cfg,
        notify_mult,
        cmd_phys,
        core::mem::size_of::<VirtioGpuResourceFlush>(),
        resp_phys,
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );
    if !submitted {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(resp) };
    if resp_header.type_ != VIRTIO_GPU_RESP_OK_NODATA {
        free_page_frame(cmd_phys);
        free_page_frame(resp_phys);
        return false;
    }

    free_page_frame(cmd_phys);
    free_page_frame(resp_phys);
    true
}

fn virtio_gpu_probe(info: *const PciDeviceInfo, _context: *mut c_void) -> c_int {
    let info = unsafe { &*info };
    unsafe {
        if VIRTIO_GPU_DEVICE.present != 0 {
            klog_debug!("PCI: virtio-gpu driver already claimed a device");
            return -1;
        }
    }

    let mut bar_opt: Option<&PciBarInfo> = None;
    for i in 0..info.bar_count as usize {
        let bar = &info.bars[i];
        if bar.is_io == 0 && bar.base != 0 {
            bar_opt = Some(bar);
            break;
        }
    }

    let bar = match bar_opt {
        Some(b) => b,
        None => {
            klog_info!("PCI: virtio-gpu missing MMIO BAR");
            wl_currency::award_loss();
            return -1;
        }
    };

    let mmio_size = if bar.size != 0 {
        bar.size as usize
    } else {
        VIRTIO_MMIO_DEFAULT_SIZE
    };
    let mmio_base = mm_map_mmio_region(bar.base, mmio_size);
    if mmio_base.is_null() {
        klog_info!(
            "PCI: virtio-gpu MMIO mapping failed for phys=0x{:x}",
            bar.base
        );
        wl_currency::award_loss();
        return -1;
    }

    virtio_gpu_enable_master(info);

    let mut caps = virtio_gpu_parse_caps(info);
    let mut handshake_ok = false;

    if !caps.common_cfg.is_null() {
        if virtio_gpu_negotiate_features(&mut caps) {
            handshake_ok = true;
            klog_debug!("PCI: virtio-gpu modern capability handshake ok");
        } else {
            klog_info!("PCI: virtio-gpu modern handshake failed");
            wl_currency::award_loss();
        }
        if handshake_ok {
            if !virtio_gpu_setup_control_queue(&mut caps) {
                klog_info!("PCI: virtio-gpu control queue setup failed");
                // Recoverable failure: queue setup failed.
                wl_currency::award_loss();
                caps.supports_virgl = 0;
            }
        }
    } else {
        let status_before = pci_config_read8(
            info.bus,
            info.device,
            info.function,
            VIRTIO_PCI_STATUS_OFFSET,
        );
        klog_debug!("PCI: virtio-gpu status read=0x{:02x}", status_before);

        pci_config_write8(
            info.bus,
            info.device,
            info.function,
            VIRTIO_PCI_STATUS_OFFSET,
            0x00,
        );
        let status_zeroed = pci_config_read8(
            info.bus,
            info.device,
            info.function,
            VIRTIO_PCI_STATUS_OFFSET,
        );
        klog_debug!("PCI: virtio-gpu status after clear=0x{:02x}", status_zeroed);

        let handshake = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
        pci_config_write8(
            info.bus,
            info.device,
            info.function,
            VIRTIO_PCI_STATUS_OFFSET,
            handshake,
        );
        let status_handshake = pci_config_read8(
            info.bus,
            info.device,
            info.function,
            VIRTIO_PCI_STATUS_OFFSET,
        );
        if (status_handshake & handshake) != handshake {
            klog_info!(
                "PCI: virtio-gpu handshake incomplete (status=0x{:02x})",
                status_handshake
            );
            mm_unmap_mmio_region(mmio_base, mmio_size);
            wl_currency::award_loss();
            return -1;
        }
        handshake_ok = true;
    }

    let sample_value = unsafe { core::ptr::read_volatile(mmio_base as *const u32) };
    klog_debug!("PCI: virtio-gpu MMIO sample value=0x{:08x}", sample_value);

    if handshake_ok {
        klog_info!("PCI: virtio-gpu driver probe succeeded (wheel gave a W)");
        if caps.supports_virgl != 0 {
            klog_info!("PCI: virtio-gpu reports virgl feature support");
        }
        wl_currency::award_win();
        if caps.ctrl_queue.ready != 0 {
            if virtio_gpu_get_display_info(&mut caps) {
                // Successful display query earns a W for the driver path.
                wl_currency::award_win();
            } else {
                // Recoverable failure: display info query failed.
                wl_currency::award_loss();
            }
            if caps.supports_virgl != 0 {
                if virtio_gpu_get_capset_info(&mut caps) {
                    wl_currency::award_win();
                } else {
                    wl_currency::award_loss();
                }
                if virtio_gpu_create_context(&mut caps, 1) {
                    caps.virgl_ready = 1;
                    klog_info!("PCI: virtio-gpu virgl context ready");
                    wl_currency::award_win();
                } else {
                    wl_currency::award_loss();
                }
            }
        }
        unsafe {
            VIRTIO_GPU_DEVICE.present = 1;
            VIRTIO_GPU_DEVICE.device = *info;
            VIRTIO_GPU_DEVICE.mmio_base = mmio_base;
            VIRTIO_GPU_DEVICE.mmio_size = mmio_size;
            VIRTIO_GPU_DEVICE.common_cfg = caps.common_cfg;
            VIRTIO_GPU_DEVICE.notify_cfg = caps.notify_cfg;
            VIRTIO_GPU_DEVICE.notify_off_multiplier = caps.notify_off_multiplier;
            VIRTIO_GPU_DEVICE.isr_cfg = caps.isr_cfg;
            VIRTIO_GPU_DEVICE.device_cfg = caps.device_cfg;
            VIRTIO_GPU_DEVICE.device_cfg_len = caps.device_cfg_len;
            VIRTIO_GPU_DEVICE.supports_virgl = caps.supports_virgl;
            VIRTIO_GPU_DEVICE.modern_caps = caps.modern_caps;
            VIRTIO_GPU_DEVICE.ctrl_queue = caps.ctrl_queue;
            VIRTIO_GPU_DEVICE.display_width = caps.display_width;
            VIRTIO_GPU_DEVICE.display_height = caps.display_height;
            VIRTIO_GPU_DEVICE.virgl_ready = caps.virgl_ready;
        }
        return 0;
    }

    mm_unmap_mmio_region(mmio_base, mmio_size);
    -1
}

static VIRTIO_GPU_PCI_DRIVER: PciDriver = PciDriver {
    name: b"virtio-gpu\0".as_ptr(),
    match_fn: Some(virtio_gpu_match),
    probe: Some(virtio_gpu_probe),
    context: core::ptr::null_mut(),
};
pub fn virtio_gpu_register_driver() {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    if pci_register_driver(&VIRTIO_GPU_PCI_DRIVER as *const PciDriver) != 0 {
        klog_info!("PCI: virtio-gpu driver registration failed");
    }
}
pub fn virtio_gpu_get_device() -> *const virtio_gpu_device_t {
    unsafe {
        if VIRTIO_GPU_DEVICE.present != 0 {
            &VIRTIO_GPU_DEVICE as *const virtio_gpu_device_t
        } else {
            core::ptr::null()
        }
    }
}

pub fn virtio_gpu_supports_virgl() -> bool {
    unsafe { VIRTIO_GPU_DEVICE.present != 0 && VIRTIO_GPU_DEVICE.supports_virgl != 0 }
}

pub fn virtio_gpu_has_modern_caps() -> bool {
    unsafe { VIRTIO_GPU_DEVICE.present != 0 && VIRTIO_GPU_DEVICE.modern_caps != 0 }
}

pub fn virtio_gpu_is_virgl_ready() -> bool {
    unsafe { VIRTIO_GPU_DEVICE.present != 0 && VIRTIO_GPU_DEVICE.virgl_ready != 0 }
}

pub fn virtio_gpu_framebuffer_init() -> Option<FramebufferInfo> {
    unsafe {
        if VIRTIO_GPU_DEVICE.present == 0 || VIRTIO_GPU_DEVICE.ctrl_queue.ready == 0 {
            return None;
        }

        if VIRTIO_GPU_DEVICE.fb_ready != 0 {
            return Some(FramebufferInfo {
                address: VIRTIO_GPU_DEVICE.fb_phys as *mut u8,
                width: VIRTIO_GPU_DEVICE.fb_width as u64,
                height: VIRTIO_GPU_DEVICE.fb_height as u64,
                pitch: VIRTIO_GPU_DEVICE.fb_pitch as u64,
                bpp: VIRTIO_GPU_DEVICE.fb_bpp,
            });
        }

        let width = VIRTIO_GPU_DEVICE.display_width;
        let height = VIRTIO_GPU_DEVICE.display_height;
        if width == 0 || height == 0 {
            // Recoverable failure: no display geometry reported.
            wl_currency::award_loss();
            return None;
        }

        let pitch = width.saturating_mul(4);
        let size = (pitch as u64).saturating_mul(height as u64);
        if size == 0 {
            // Recoverable failure: invalid framebuffer size.
            wl_currency::award_loss();
            return None;
        }

        let size_aligned = align_up(size as usize, PAGE_SIZE_4KB as usize) as u64;
        let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
        let phys = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
        if phys == 0 {
            // Recoverable failure: backing store allocation failed.
            wl_currency::award_loss();
            return None;
        }

        let resource_id = if VIRTIO_GPU_DEVICE.fb_resource_id != 0 {
            VIRTIO_GPU_DEVICE.fb_resource_id
        } else {
            1
        };

        if !virtio_gpu_resource_create_2d(&mut VIRTIO_GPU_DEVICE, resource_id, width, height) {
            free_page_frame(phys);
            // Recoverable failure: resource creation failed.
            wl_currency::award_loss();
            return None;
        }

        if !virtio_gpu_resource_attach_backing(&mut VIRTIO_GPU_DEVICE, resource_id, phys, size as u32) {
            free_page_frame(phys);
            // Recoverable failure: backing attach failed.
            wl_currency::award_loss();
            return None;
        }

        if !virtio_gpu_set_scanout(&mut VIRTIO_GPU_DEVICE, resource_id, width, height) {
            free_page_frame(phys);
            // Recoverable failure: scanout bind failed.
            wl_currency::award_loss();
            return None;
        }

        if !virtio_gpu_transfer_to_host_2d(&mut VIRTIO_GPU_DEVICE, resource_id, width, height)
            || !virtio_gpu_resource_flush(&mut VIRTIO_GPU_DEVICE, resource_id, width, height)
        {
            free_page_frame(phys);
            // Recoverable failure: initial transfer/flush failed.
            wl_currency::award_loss();
            return None;
        }

        VIRTIO_GPU_DEVICE.fb_resource_id = resource_id;
        VIRTIO_GPU_DEVICE.fb_phys = phys;
        VIRTIO_GPU_DEVICE.fb_size = size;
        VIRTIO_GPU_DEVICE.fb_width = width;
        VIRTIO_GPU_DEVICE.fb_height = height;
        VIRTIO_GPU_DEVICE.fb_pitch = pitch;
        VIRTIO_GPU_DEVICE.fb_bpp = 32;
        VIRTIO_GPU_DEVICE.fb_ready = 1;

        // Successful framebuffer bring-up earns a W.
        wl_currency::award_win();

        Some(FramebufferInfo {
            address: phys as *mut u8,
            width: width as u64,
            height: height as u64,
            pitch: pitch as u64,
            bpp: 32,
        })
    }
}

pub fn virtio_gpu_flush_full() -> c_int {
    unsafe {
        if VIRTIO_GPU_DEVICE.fb_ready == 0 {
            return -1;
        }
        let width = VIRTIO_GPU_DEVICE.fb_width;
        let height = VIRTIO_GPU_DEVICE.fb_height;
        let resource_id = VIRTIO_GPU_DEVICE.fb_resource_id;
        if resource_id == 0 || width == 0 || height == 0 {
            // Recoverable failure: framebuffer metadata missing.
            wl_currency::award_loss();
            return -1;
        }
        if !virtio_gpu_transfer_to_host_2d(&mut VIRTIO_GPU_DEVICE, resource_id, width, height) {
            // Recoverable failure: transfer to host failed.
            wl_currency::award_loss();
            return -1;
        }
        if !virtio_gpu_resource_flush(&mut VIRTIO_GPU_DEVICE, resource_id, width, height) {
            // Recoverable failure: resource flush failed.
            wl_currency::award_loss();
            return -1;
        }

        // Successful flush earns a W.
        wl_currency::award_win();
        0
    }
}
