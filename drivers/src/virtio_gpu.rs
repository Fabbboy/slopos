#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{FramebufferInfo, align_up, klog_debug, klog_info};

use crate::pci::{
    PciBarInfo, PciDeviceInfo, PciDriver, pci_config_read8, pci_config_read16, pci_config_read32,
    pci_config_write8, pci_config_write16, pci_register_driver,
};
use slopos_abi::arch::x86_64::pci::{
    PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET,
};

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, alloc_page_frames, free_page_frame,
};

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

trait GpuCommand: Sized {
    const CMD_TYPE: u32;
    const EXPECTED_RESP: u32 = VIRTIO_GPU_RESP_OK_NODATA;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader;

    fn init_header(&mut self) {
        *self.header_mut() = VirtioGpuCtrlHeader {
            type_: Self::CMD_TYPE,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };
    }
}

impl GpuCommand for VirtioGpuCtrlHeader {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_GET_DISPLAY_INFO;
    const EXPECTED_RESP: u32 = VIRTIO_GPU_RESP_OK_DISPLAY_INFO;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        self
    }
}

impl GpuCommand for VirtioGpuGetCapsetInfo {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_GET_CAPSET_INFO;
    const EXPECTED_RESP: u32 = VIRTIO_GPU_RESP_OK_CAPSET_INFO;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuCtxCreate {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_CTX_CREATE;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuResourceCreate2d {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_RESOURCE_CREATE_2D;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuResourceAttachBacking {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuSetScanout {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_SET_SCANOUT;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuTransferToHost2d {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

impl GpuCommand for VirtioGpuResourceFlush {
    const CMD_TYPE: u32 = VIRTIO_GPU_CMD_RESOURCE_FLUSH;

    fn header_mut(&mut self) -> &mut VirtioGpuCtrlHeader {
        &mut self.header
    }
}

struct CmdBuffer {
    cmd_phys: PhysAddr,
    resp_phys: PhysAddr,
}

impl CmdBuffer {
    fn new() -> Option<Self> {
        let cmd_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
        let resp_phys = alloc_page_frame(ALLOC_FLAG_ZERO);

        if cmd_phys.is_null() || resp_phys.is_null() {
            if !cmd_phys.is_null() {
                free_page_frame(cmd_phys);
            }
            if !resp_phys.is_null() {
                free_page_frame(resp_phys);
            }
            return None;
        }

        let cmd_virt = cmd_phys.to_virt().as_u64();
        let resp_virt = resp_phys.to_virt().as_u64();
        if cmd_virt == 0 || resp_virt == 0 {
            free_page_frame(cmd_phys);
            free_page_frame(resp_phys);
            return None;
        }

        Some(Self {
            cmd_phys,
            resp_phys,
        })
    }

    fn cmd_mut<T>(&self) -> &mut T {
        unsafe { &mut *(self.cmd_phys.to_virt().as_u64() as *mut T) }
    }

    fn resp<T>(&self) -> &T {
        unsafe { &*(self.resp_phys.to_virt().as_u64() as *const T) }
    }

    fn cmd_phys(&self) -> u64 {
        self.cmd_phys.as_u64()
    }

    fn resp_phys(&self) -> u64 {
        self.resp_phys.as_u64()
    }

    fn cmd_virt(&self) -> u64 {
        self.cmd_phys.to_virt().as_u64()
    }
}

impl Drop for CmdBuffer {
    fn drop(&mut self) {
        free_page_frame(self.cmd_phys);
        free_page_frame(self.resp_phys);
    }
}

fn execute_cmd<C: GpuCommand>(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    init: impl FnOnce(&mut C),
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let notify_cfg = match mmio.notify_cfg {
        Some(cfg) => cfg,
        None => return false,
    };

    let buf = match CmdBuffer::new() {
        Some(b) => b,
        None => return false,
    };

    let cmd: &mut C = buf.cmd_mut();
    cmd.init_header();
    init(cmd);

    unsafe {
        core::ptr::write_volatile(buf.cmd_mut::<C>(), core::ptr::read(cmd));
    }

    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        &notify_cfg,
        device.notify_off_multiplier,
        buf.cmd_phys(),
        core::mem::size_of::<C>(),
        buf.resp_phys(),
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );

    if !submitted {
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(buf.resp::<VirtioGpuCtrlHeader>()) };
    resp_header.type_ == C::EXPECTED_RESP
}

/// Execute a GPU command and provide access to the response buffer.
/// Returns `Some(R)` if the command succeeds, `None` otherwise.
fn execute_cmd_with_response<C: GpuCommand, R, F>(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resp_size: usize,
    init: impl FnOnce(&mut C),
    read_response: F,
) -> Option<R>
where
    F: FnOnce(&CmdBuffer) -> R,
{
    if device.ctrl_queue.ready == 0 {
        return None;
    }

    let notify_cfg = mmio.notify_cfg?;

    let buf = CmdBuffer::new()?;

    let cmd: &mut C = buf.cmd_mut();
    cmd.init_header();
    init(cmd);

    unsafe {
        core::ptr::write_volatile(buf.cmd_mut::<C>(), core::ptr::read(cmd));
    }

    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        &notify_cfg,
        device.notify_off_multiplier,
        buf.cmd_phys(),
        core::mem::size_of::<C>(),
        buf.resp_phys(),
        resp_size,
    );

    if !submitted {
        return None;
    }

    let resp_header = unsafe { core::ptr::read_volatile(buf.resp::<VirtioGpuCtrlHeader>()) };
    if resp_header.type_ != C::EXPECTED_RESP {
        return None;
    }

    Some(read_response(&buf))
}

fn execute_cmd_with_extra<C: GpuCommand>(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    extra_len: usize,
    init: impl FnOnce(&mut C),
    write_extra: impl FnOnce(*mut u8),
) -> bool {
    if device.ctrl_queue.ready == 0 {
        return false;
    }

    let notify_cfg = match mmio.notify_cfg {
        Some(cfg) => cfg,
        None => return false,
    };

    let buf = match CmdBuffer::new() {
        Some(b) => b,
        None => return false,
    };

    let cmd: &mut C = buf.cmd_mut();
    cmd.init_header();
    init(cmd);

    unsafe {
        core::ptr::write_volatile(buf.cmd_mut::<C>(), core::ptr::read(cmd));
    }

    let extra_ptr = unsafe { (buf.cmd_virt() as *mut u8).add(core::mem::size_of::<C>()) };
    write_extra(extra_ptr);

    let cmd_len = core::mem::size_of::<C>() + extra_len;
    let submitted = virtio_gpu_send_cmd(
        &mut device.ctrl_queue,
        &notify_cfg,
        device.notify_off_multiplier,
        buf.cmd_phys(),
        cmd_len,
        buf.resp_phys(),
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    );

    if !submitted {
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(buf.resp::<VirtioGpuCtrlHeader>()) };
    resp_header.type_ == C::EXPECTED_RESP
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

#[derive(Clone, Copy, Default)]
struct VirtioGpuMmioCaps {
    common_cfg: Option<MmioRegion>,
    notify_cfg: Option<MmioRegion>,
    isr_cfg: Option<MmioRegion>,
    device_cfg: Option<MmioRegion>,
    device_cfg_len: u32,
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

static mut VIRTIO_GPU_MMIO: VirtioGpuMmioCaps = VirtioGpuMmioCaps {
    common_cfg: None,
    notify_cfg: None,
    isr_cfg: None,
    device_cfg: None,
    device_cfg_len: 0,
};

const COMMON_CFG_DEVICE_FEATURE_SELECT: usize =
    core::mem::offset_of!(VirtioPciCommonCfg, device_feature_select);
const COMMON_CFG_DEVICE_FEATURE: usize = core::mem::offset_of!(VirtioPciCommonCfg, device_feature);
const COMMON_CFG_DRIVER_FEATURE_SELECT: usize =
    core::mem::offset_of!(VirtioPciCommonCfg, driver_feature_select);
const COMMON_CFG_DRIVER_FEATURE: usize = core::mem::offset_of!(VirtioPciCommonCfg, driver_feature);
const COMMON_CFG_DEVICE_STATUS: usize = core::mem::offset_of!(VirtioPciCommonCfg, device_status);
const COMMON_CFG_QUEUE_SELECT: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_select);
const COMMON_CFG_QUEUE_SIZE: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_size);
const COMMON_CFG_QUEUE_DESC: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_desc);
const COMMON_CFG_QUEUE_AVAIL: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_avail);
const COMMON_CFG_QUEUE_USED: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_used);
const COMMON_CFG_QUEUE_ENABLE: usize = core::mem::offset_of!(VirtioPciCommonCfg, queue_enable);
const COMMON_CFG_QUEUE_NOTIFY_OFF: usize =
    core::mem::offset_of!(VirtioPciCommonCfg, queue_notify_off);

fn virtio_gpu_mmio_caps() -> Option<VirtioGpuMmioCaps> {
    unsafe {
        if VIRTIO_GPU_MMIO.common_cfg.is_some() || VIRTIO_GPU_MMIO.notify_cfg.is_some() {
            Some(VIRTIO_GPU_MMIO)
        } else {
            None
        }
    }
}

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
) -> Option<MmioRegion> {
    let bar = info.bars.get(bar_index as usize);
    let bar = match bar {
        Some(b) => b,
        None => return None,
    };
    if bar.is_io != 0 || bar.base == 0 || length == 0 {
        return None;
    }
    let phys = bar.base.wrapping_add(offset as u64);
    MmioRegion::map(PhysAddr::new(phys), length as usize)
}

fn virtio_gpu_parse_caps(info: &PciDeviceInfo) -> (virtio_gpu_device_t, VirtioGpuMmioCaps) {
    let mut caps = virtio_gpu_device_t::default();
    let mut mmio_caps = VirtioGpuMmioCaps::default();

    let status = pci_config_read16(info.bus, info.device, info.function, PCI_STATUS_OFFSET);
    if (status & PCI_STATUS_CAP_LIST) == 0 {
        return (caps, mmio_caps);
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
            if let Some(region) = mapped {
                let mapped_ptr = region.virt_base() as *mut u8;
                match cfg_type {
                    VIRTIO_PCI_CAP_COMMON_CFG => {
                        caps.common_cfg = mapped_ptr as *mut VirtioPciCommonCfg;
                        mmio_caps.common_cfg = Some(region);
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                        caps.notify_cfg = mapped_ptr;
                        mmio_caps.notify_cfg = Some(region);
                        caps.notify_off_multiplier =
                            pci_config_read32(info.bus, info.device, info.function, cap_ptr + 16);
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_ISR_CFG => {
                        caps.isr_cfg = mapped_ptr;
                        mmio_caps.isr_cfg = Some(region);
                        caps.modern_caps = 1;
                    }
                    VIRTIO_PCI_CAP_DEVICE_CFG => {
                        caps.device_cfg = mapped_ptr;
                        caps.device_cfg_len = length;
                        mmio_caps.device_cfg = Some(region);
                        mmio_caps.device_cfg_len = length;
                        caps.modern_caps = 1;
                    }
                    _ => {}
                }
            }
        }
        cap_ptr = cap_next;
    }

    (caps, mmio_caps)
}

fn virtio_gpu_set_status(cfg: &MmioRegion, status: u8) {
    cfg.write_u8(COMMON_CFG_DEVICE_STATUS, status);
}

fn virtio_gpu_get_status(cfg: &MmioRegion) -> u8 {
    cfg.read_u8(COMMON_CFG_DEVICE_STATUS)
}

fn virtio_gpu_negotiate_features(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
) -> bool {
    let cfg = match mmio.common_cfg {
        Some(cfg) => cfg,
        None => return false,
    };

    virtio_gpu_set_status(&cfg, 0);
    let mut status = virtio_gpu_get_status(&cfg);
    status |= VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
    virtio_gpu_set_status(&cfg, status);

    cfg.write_u32(COMMON_CFG_DEVICE_FEATURE_SELECT, 0);
    let features_low = cfg.read_u32(COMMON_CFG_DEVICE_FEATURE);

    cfg.write_u32(COMMON_CFG_DEVICE_FEATURE_SELECT, 1);
    let features_high = cfg.read_u32(COMMON_CFG_DEVICE_FEATURE);

    let driver_features_low = features_low & VIRTIO_GPU_F_VIRGL;
    let driver_features_high = features_high & VIRTIO_F_VERSION_1;

    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE_SELECT, 0);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE, driver_features_low);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE_SELECT, 1);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE, driver_features_high);

    status |= VIRTIO_STATUS_FEATURES_OK;
    virtio_gpu_set_status(&cfg, status);
    status = virtio_gpu_get_status(&cfg);
    if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
        return false;
    }

    if (features_low & VIRTIO_GPU_F_VIRGL) != 0 {
        device.supports_virgl = 1;
    }

    true
}

fn virtio_gpu_setup_control_queue(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
) -> bool {
    let cfg = match mmio.common_cfg {
        Some(cfg) => cfg,
        None => return false,
    };
    if mmio.notify_cfg.is_none() {
        return false;
    }

    cfg.write_u32(COMMON_CFG_QUEUE_SELECT, VIRTIO_GPU_QUEUE_CONTROL as u32);
    let queue_size = cfg.read_u16(COMMON_CFG_QUEUE_SIZE);
    if queue_size == 0 {
        return false;
    }
    let size = core::cmp::min(queue_size, VIRTIO_GPU_QUEUE_SIZE);

    let desc_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let avail_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    let used_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if desc_phys.is_null() || avail_phys.is_null() || used_phys.is_null() {
        if !desc_phys.is_null() {
            free_page_frame(desc_phys);
        }
        if !avail_phys.is_null() {
            free_page_frame(avail_phys);
        }
        if !used_phys.is_null() {
            free_page_frame(used_phys);
        }
        return false;
    }

    let desc_virt = desc_phys.to_virt().as_u64();
    let avail_virt = avail_phys.to_virt().as_u64();
    let used_virt = used_phys.to_virt().as_u64();
    if desc_virt == 0 || avail_virt == 0 || used_virt == 0 {
        free_page_frame(desc_phys);
        free_page_frame(avail_phys);
        free_page_frame(used_phys);
        return false;
    }

    cfg.write_u64(COMMON_CFG_QUEUE_DESC, desc_phys.as_u64());
    cfg.write_u64(COMMON_CFG_QUEUE_AVAIL, avail_phys.as_u64());
    cfg.write_u64(COMMON_CFG_QUEUE_USED, used_phys.as_u64());
    cfg.write_u16(COMMON_CFG_QUEUE_ENABLE, 1);

    let notify_off = cfg.read_u16(COMMON_CFG_QUEUE_NOTIFY_OFF);

    device.ctrl_queue = VirtioGpuQueue {
        size,
        desc_phys: desc_phys.as_u64(),
        avail_phys: avail_phys.as_u64(),
        used_phys: used_phys.as_u64(),
        desc: desc_virt as *mut VirtqDesc,
        avail: avail_virt as *mut VirtqAvail,
        used: used_virt as *mut VirtqUsed,
        notify_off,
        last_used_idx: 0,
        ready: 1,
    };

    let mut status = virtio_gpu_get_status(&cfg);
    status |= VIRTIO_STATUS_DRIVER_OK;
    virtio_gpu_set_status(&cfg, status);

    true
}

fn virtio_gpu_queue_notify(
    notify_cfg: &MmioRegion,
    notify_off_multiplier: u32,
    queue: &VirtioGpuQueue,
) {
    let offset = (queue.notify_off as u32 * notify_off_multiplier) as usize;
    notify_cfg.write_u16(offset, VIRTIO_GPU_QUEUE_CONTROL);
}

fn virtio_gpu_send_cmd(
    queue: &mut VirtioGpuQueue,
    notify_cfg: &MmioRegion,
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
    notify_cfg: &MmioRegion,
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

fn virtio_gpu_get_display_info(device: &mut virtio_gpu_device_t, mmio: &VirtioGpuMmioCaps) -> bool {
    let display = execute_cmd_with_response::<VirtioGpuCtrlHeader, Option<VirtioGpuDisplayOne>, _>(
        device,
        mmio,
        core::mem::size_of::<VirtioGpuRespDisplayInfo>(),
        |_cmd| {},
        |buf| {
            let resp = buf.resp::<VirtioGpuRespDisplayInfo>();
            let display = unsafe { core::ptr::read_volatile(&(*resp).displays[0]) };
            if display.enabled != 0 {
                Some(display)
            } else {
                None
            }
        },
    );

    if let Some(Some(display)) = display {
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

    display.is_some()
}

fn virtio_gpu_get_capset_info(device: &mut virtio_gpu_device_t, mmio: &VirtioGpuMmioCaps) -> bool {
    let capset_info = execute_cmd_with_response::<VirtioGpuGetCapsetInfo, (u32, u32, u32), _>(
        device,
        mmio,
        core::mem::size_of::<VirtioGpuRespCapsetInfo>(),
        |cmd| {
            cmd.capset_index = 0;
            cmd.padding = 0;
        },
        |buf| {
            let resp = buf.resp::<VirtioGpuRespCapsetInfo>();
            unsafe {
                (
                    core::ptr::read_volatile(&(*resp).capset_id),
                    core::ptr::read_volatile(&(*resp).capset_max_version),
                    core::ptr::read_volatile(&(*resp).capset_max_size),
                )
            }
        },
    );

    if let Some((capset_id, capset_version, capset_size)) = capset_info {
        klog_info!(
            "PCI: virtio-gpu capset id {} (max ver {}, max size {})",
            capset_id,
            capset_version,
            capset_size
        );
        if capset_id == VIRTIO_GPU_CAPSET_VIRGL || capset_id == VIRTIO_GPU_CAPSET_VIRGL2 {
            klog_info!("PCI: virtio-gpu virgl capset detected");
        }
        true
    } else {
        false
    }
}

fn virtio_gpu_create_context(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    ctx_id: u32,
) -> bool {
    execute_cmd::<VirtioGpuCtxCreate>(device, mmio, |cmd| {
        cmd.ctx_id = ctx_id;
        cmd.name_len = 0;
        cmd.padding0 = 0;
        cmd.padding1 = 0;
    })
}

fn virtio_gpu_resource_create_2d(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    execute_cmd::<VirtioGpuResourceCreate2d>(device, mmio, |cmd| {
        cmd.resource_id = resource_id;
        cmd.format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM;
        cmd.width = width;
        cmd.height = height;
    })
}

fn virtio_gpu_resource_attach_backing(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resource_id: u32,
    backing_phys: u64,
    backing_len: u32,
) -> bool {
    execute_cmd_with_extra::<VirtioGpuResourceAttachBacking>(
        device,
        mmio,
        core::mem::size_of::<VirtioGpuMemEntry>(),
        |cmd| {
            cmd.resource_id = resource_id;
            cmd.nr_entries = 1;
        },
        |extra_ptr| unsafe {
            core::ptr::write_volatile(
                extra_ptr as *mut VirtioGpuMemEntry,
                VirtioGpuMemEntry {
                    addr: backing_phys,
                    length: backing_len,
                    padding: 0,
                },
            );
        },
    )
}

fn virtio_gpu_set_scanout(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    execute_cmd::<VirtioGpuSetScanout>(device, mmio, |cmd| {
        cmd.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        cmd.scanout_id = 0;
        cmd.resource_id = resource_id;
    })
}

fn virtio_gpu_transfer_to_host_2d(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    execute_cmd::<VirtioGpuTransferToHost2d>(device, mmio, |cmd| {
        cmd.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        cmd.offset = 0;
        cmd.resource_id = resource_id;
        cmd.padding = 0;
    })
}

fn virtio_gpu_resource_flush(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioGpuMmioCaps,
    resource_id: u32,
    width: u32,
    height: u32,
) -> bool {
    execute_cmd::<VirtioGpuResourceFlush>(device, mmio, |cmd| {
        cmd.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        cmd.resource_id = resource_id;
        cmd.padding = 0;
    })
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
            return -1;
        }
    };

    let mmio_size = if bar.size != 0 {
        bar.size as usize
    } else {
        VIRTIO_MMIO_DEFAULT_SIZE
    };
    let mmio_region = MmioRegion::map(PhysAddr::new(bar.base), mmio_size);
    let mmio_base = mmio_region
        .as_ref()
        .map(|r| r.virt_base() as *mut core::ffi::c_void)
        .unwrap_or(core::ptr::null_mut());
    if mmio_base.is_null() {
        klog_info!(
            "PCI: virtio-gpu MMIO mapping failed for phys=0x{:x}",
            bar.base
        );
        return -1;
    }

    virtio_gpu_enable_master(info);

    let (mut caps, mmio_caps) = virtio_gpu_parse_caps(info);
    let mut handshake_ok = false;

    if !caps.common_cfg.is_null() {
        if virtio_gpu_negotiate_features(&mut caps, &mmio_caps) {
            handshake_ok = true;
            klog_debug!("PCI: virtio-gpu modern capability handshake ok");
        } else {
            klog_info!("PCI: virtio-gpu modern handshake failed");
        }
        if handshake_ok {
            if !virtio_gpu_setup_control_queue(&mut caps, &mmio_caps) {
                klog_info!("PCI: virtio-gpu control queue setup failed");
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
            return -1;
        }
        handshake_ok = true;
    }

    let sample_value = mmio_region.as_ref().map(|r| r.read_u32(0)).unwrap_or(0);
    klog_debug!("PCI: virtio-gpu MMIO sample value=0x{:08x}", sample_value);

    if handshake_ok {
        klog_info!("PCI: virtio-gpu driver probe succeeded");
        if caps.supports_virgl != 0 {
            klog_info!("PCI: virtio-gpu reports virgl feature support");
        }
        if caps.ctrl_queue.ready != 0 {
            let _ = virtio_gpu_get_display_info(&mut caps, &mmio_caps);
            if caps.supports_virgl != 0 {
                let _ = virtio_gpu_get_capset_info(&mut caps, &mmio_caps);
                if virtio_gpu_create_context(&mut caps, &mmio_caps, 1) {
                    caps.virgl_ready = 1;
                    klog_info!("PCI: virtio-gpu virgl context ready");
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
            VIRTIO_GPU_MMIO = mmio_caps;
        }
        return 0;
    }

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
    if pci_register_driver(&VIRTIO_GPU_PCI_DRIVER) != 0 {
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
            return None;
        }

        let pitch = width.saturating_mul(4);
        let size = (pitch as u64).saturating_mul(height as u64);
        if size == 0 {
            // Recoverable failure: invalid framebuffer size.
            return None;
        }

        let size_aligned = align_up(size as usize, PAGE_SIZE_4KB as usize) as u64;
        let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
        let phys = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
        if phys.is_null() {
            // Recoverable failure: backing store allocation failed.
            return None;
        }

        let resource_id = if VIRTIO_GPU_DEVICE.fb_resource_id != 0 {
            VIRTIO_GPU_DEVICE.fb_resource_id
        } else {
            1
        };

        let Some(mmio) = virtio_gpu_mmio_caps() else {
            return None;
        };

        if !virtio_gpu_resource_create_2d(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height)
        {
            free_page_frame(phys);
            // Recoverable failure: resource creation failed.
            return None;
        }

        if !virtio_gpu_resource_attach_backing(
            &mut VIRTIO_GPU_DEVICE,
            &mmio,
            resource_id,
            phys.as_u64(),
            size as u32,
        ) {
            free_page_frame(phys);
            // Recoverable failure: backing attach failed.
            return None;
        }

        if !virtio_gpu_set_scanout(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height) {
            free_page_frame(phys);
            // Recoverable failure: scanout bind failed.
            return None;
        }

        if !virtio_gpu_transfer_to_host_2d(
            &mut VIRTIO_GPU_DEVICE,
            &mmio,
            resource_id,
            width,
            height,
        ) || !virtio_gpu_resource_flush(
            &mut VIRTIO_GPU_DEVICE,
            &mmio,
            resource_id,
            width,
            height,
        ) {
            free_page_frame(phys);
            // Recoverable failure: initial transfer/flush failed.
            return None;
        }

        VIRTIO_GPU_DEVICE.fb_resource_id = resource_id;
        VIRTIO_GPU_DEVICE.fb_phys = phys.as_u64();
        VIRTIO_GPU_DEVICE.fb_size = size;
        VIRTIO_GPU_DEVICE.fb_width = width;
        VIRTIO_GPU_DEVICE.fb_height = height;
        VIRTIO_GPU_DEVICE.fb_pitch = pitch;
        VIRTIO_GPU_DEVICE.fb_bpp = 32;
        VIRTIO_GPU_DEVICE.fb_ready = 1;

        Some(FramebufferInfo {
            address: phys.to_virt().as_mut_ptr::<u8>(),
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
            return -1;
        }
        let Some(mmio) = virtio_gpu_mmio_caps() else {
            return -1;
        };
        if !virtio_gpu_transfer_to_host_2d(
            &mut VIRTIO_GPU_DEVICE,
            &mmio,
            resource_id,
            width,
            height,
        ) {
            // Recoverable failure: transfer to host failed.
            return -1;
        }
        if !virtio_gpu_resource_flush(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height) {
            return -1;
        }

        0
    }
}
