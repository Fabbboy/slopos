#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};
use core::sync::atomic::Ordering;

use slopos_abi::{DisplayInfo, FramebufferData, PixelFormat};
use slopos_lib::{align_up, klog_debug, klog_info, InitFlag};

use crate::pci::{
    pci_config_read8, pci_config_write8, pci_register_driver, PciBarInfo, PciDeviceInfo, PciDriver,
};
use crate::virtio::{
    pci::{
        enable_bus_master, negotiate_features, parse_capabilities, set_driver_ok, VIRTIO_VENDOR_ID,
    },
    queue::{
        self, VirtqDesc, Virtqueue, DEFAULT_QUEUE_SIZE, VIRTIO_COMPLETION_COUNT,
        VIRTIO_FENCE_COUNT, VIRTIO_SPIN_COUNT,
    },
    VirtioMmioCaps, VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTQ_DESC_F_NEXT,
    VIRTQ_DESC_F_WRITE,
};

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mm_constants::{PAGE_SIZE_4KB, PAGE_SIZE_4KB_USIZE};
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{
    alloc_page_frame, alloc_page_frames, free_page_frame, ALLOC_FLAG_ZERO,
};

pub const VIRTIO_GPU_DEVICE_ID_PRIMARY: u16 = 0x1050;
pub const VIRTIO_GPU_DEVICE_ID_TRANS: u16 = 0x1010;

const VIRTIO_PCI_STATUS_OFFSET: u8 = 0x12;
const VIRTIO_GPU_F_VIRGL: u64 = 1 << 0;

const VIRTIO_MMIO_DEFAULT_SIZE: usize = PAGE_SIZE_4KB_USIZE;
const VIRTIO_GPU_QUEUE_CONTROL: u16 = 0;
const GPU_CMD_TIMEOUT_SPINS: u32 = 1_000_000;

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct virtio_gpu_device_t {
    pub present: c_int,
    pub device: PciDeviceInfo,
    pub mmio_base: *mut core::ffi::c_void,
    pub mmio_size: usize,
    pub notify_off_multiplier: u32,
    pub supports_virgl: u8,
    pub modern_caps: u8,
    ctrl_queue: Virtqueue,
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
    notify_off_multiplier: 0,
    supports_virgl: 0,
    modern_caps: 0,
    ctrl_queue: Virtqueue::new(),
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

static mut VIRTIO_GPU_MMIO: VirtioMmioCaps = VirtioMmioCaps::empty();

fn virtio_gpu_mmio_caps() -> Option<VirtioMmioCaps> {
    unsafe {
        if VIRTIO_GPU_MMIO.has_common_cfg() || VIRTIO_GPU_MMIO.has_notify_cfg() {
            Some(VIRTIO_GPU_MMIO)
        } else {
            None
        }
    }
}

fn send_gpu_cmd(
    queue: &mut Virtqueue,
    mmio: &VirtioMmioCaps,
    cmd_phys: u64,
    cmd_len: usize,
    resp_phys: u64,
    resp_len: usize,
) -> bool {
    if !queue.is_ready() || !mmio.has_notify_cfg() {
        return false;
    }

    queue.write_desc(
        0,
        VirtqDesc {
            addr: cmd_phys,
            len: cmd_len as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: 1,
        },
    );

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

    let notify_off_multiplier = unsafe { VIRTIO_GPU_DEVICE.notify_off_multiplier };
    queue::notify_queue(
        &mmio.notify_cfg,
        notify_off_multiplier,
        queue,
        VIRTIO_GPU_QUEUE_CONTROL,
    );
    queue.poll_used(GPU_CMD_TIMEOUT_SPINS)
}

fn execute_cmd<C: GpuCommand>(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioMmioCaps,
    init: impl FnOnce(&mut C),
) -> bool {
    if !device.ctrl_queue.is_ready() {
        return false;
    }

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

    if !send_gpu_cmd(
        &mut device.ctrl_queue,
        mmio,
        buf.cmd_phys(),
        core::mem::size_of::<C>(),
        buf.resp_phys(),
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    ) {
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(buf.resp::<VirtioGpuCtrlHeader>()) };
    resp_header.type_ == C::EXPECTED_RESP
}

fn execute_cmd_with_response<C: GpuCommand, R, F>(
    device: &mut virtio_gpu_device_t,
    mmio: &VirtioMmioCaps,
    resp_size: usize,
    init: impl FnOnce(&mut C),
    read_response: F,
) -> Option<R>
where
    F: FnOnce(&CmdBuffer) -> R,
{
    if !device.ctrl_queue.is_ready() {
        return None;
    }

    let buf = CmdBuffer::new()?;

    let cmd: &mut C = buf.cmd_mut();
    cmd.init_header();
    init(cmd);

    unsafe {
        core::ptr::write_volatile(buf.cmd_mut::<C>(), core::ptr::read(cmd));
    }

    if !send_gpu_cmd(
        &mut device.ctrl_queue,
        mmio,
        buf.cmd_phys(),
        core::mem::size_of::<C>(),
        buf.resp_phys(),
        resp_size,
    ) {
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
    mmio: &VirtioMmioCaps,
    extra_len: usize,
    init: impl FnOnce(&mut C),
    write_extra: impl FnOnce(*mut u8),
) -> bool {
    if !device.ctrl_queue.is_ready() {
        return false;
    }

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
    if !send_gpu_cmd(
        &mut device.ctrl_queue,
        mmio,
        buf.cmd_phys(),
        cmd_len,
        buf.resp_phys(),
        core::mem::size_of::<VirtioGpuCtrlHeader>(),
    ) {
        return false;
    }

    let resp_header = unsafe { core::ptr::read_volatile(buf.resp::<VirtioGpuCtrlHeader>()) };
    resp_header.type_ == C::EXPECTED_RESP
}

fn virtio_gpu_match(info: *const PciDeviceInfo, _context: *mut c_void) -> bool {
    let info = unsafe { &*info };
    if info.vendor_id != VIRTIO_VENDOR_ID {
        return false;
    }
    info.device_id == VIRTIO_GPU_DEVICE_ID_PRIMARY || info.device_id == VIRTIO_GPU_DEVICE_ID_TRANS
}

fn virtio_gpu_get_display_info(device: &mut virtio_gpu_device_t, mmio: &VirtioMmioCaps) -> bool {
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

fn virtio_gpu_get_capset_info(device: &mut virtio_gpu_device_t, mmio: &VirtioMmioCaps) -> bool {
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
    mmio: &VirtioMmioCaps,
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
    mmio: &VirtioMmioCaps,
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
    mmio: &VirtioMmioCaps,
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
    mmio: &VirtioMmioCaps,
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
    mmio: &VirtioMmioCaps,
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
    mmio: &VirtioMmioCaps,
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

    enable_bus_master(info);

    let caps = parse_capabilities(info);
    let mut handshake_ok = false;
    let mut supports_virgl = false;

    if caps.has_common_cfg() {
        let feat_result =
            negotiate_features(&caps, crate::virtio::VIRTIO_F_VERSION_1, VIRTIO_GPU_F_VIRGL);
        if feat_result.success {
            handshake_ok = true;
            supports_virgl = (feat_result.driver_features & VIRTIO_GPU_F_VIRGL) != 0;
            klog_debug!("PCI: virtio-gpu modern capability handshake ok");
        } else {
            klog_info!("PCI: virtio-gpu modern handshake failed");
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

    if !handshake_ok {
        return -1;
    }

    let ctrl_queue = if caps.has_common_cfg() && caps.has_notify_cfg() {
        match queue::setup_queue(
            &caps.common_cfg,
            VIRTIO_GPU_QUEUE_CONTROL,
            DEFAULT_QUEUE_SIZE,
        ) {
            Some(q) => {
                set_driver_ok(&caps);
                Some(q)
            }
            None => {
                klog_info!("PCI: virtio-gpu control queue setup failed");
                None
            }
        }
    } else {
        None
    };

    let sample_value = mmio_region.as_ref().map(|r| r.read_u32(0)).unwrap_or(0);
    klog_debug!("PCI: virtio-gpu MMIO sample value=0x{:08x}", sample_value);

    klog_info!("PCI: virtio-gpu driver probe succeeded");
    if supports_virgl {
        klog_info!("PCI: virtio-gpu reports virgl feature support");
    }

    let mut dev = virtio_gpu_device_t {
        present: 1,
        device: *info,
        mmio_base,
        mmio_size,
        notify_off_multiplier: caps.notify_off_multiplier,
        supports_virgl: if supports_virgl { 1 } else { 0 },
        modern_caps: if caps.has_common_cfg() { 1 } else { 0 },
        ctrl_queue: ctrl_queue.unwrap_or(Virtqueue::new()),
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

    if ctrl_queue.is_some() {
        let _ = virtio_gpu_get_display_info(&mut dev, &caps);
        if supports_virgl {
            let _ = virtio_gpu_get_capset_info(&mut dev, &caps);
            if virtio_gpu_create_context(&mut dev, &caps, 1) {
                dev.virgl_ready = 1;
                klog_info!("PCI: virtio-gpu virgl context ready");
            }
        }
    }

    unsafe {
        VIRTIO_GPU_DEVICE = dev;
        VIRTIO_GPU_MMIO = caps;
    }

    0
}

static VIRTIO_GPU_PCI_DRIVER: PciDriver = PciDriver {
    name: b"virtio-gpu\0".as_ptr(),
    match_fn: Some(virtio_gpu_match),
    probe: Some(virtio_gpu_probe),
    context: core::ptr::null_mut(),
};

pub fn virtio_gpu_register_driver() {
    static REGISTERED: InitFlag = InitFlag::new();
    if !REGISTERED.claim() {
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

pub fn virtio_gpu_framebuffer_init() -> Option<FramebufferData> {
    unsafe {
        if VIRTIO_GPU_DEVICE.present == 0 || !VIRTIO_GPU_DEVICE.ctrl_queue.is_ready() {
            return None;
        }

        if VIRTIO_GPU_DEVICE.fb_ready != 0 {
            return Some(FramebufferData {
                address: VIRTIO_GPU_DEVICE.fb_phys as *mut u8,
                info: DisplayInfo::new(
                    VIRTIO_GPU_DEVICE.fb_width,
                    VIRTIO_GPU_DEVICE.fb_height,
                    VIRTIO_GPU_DEVICE.fb_pitch,
                    PixelFormat::from_bpp(VIRTIO_GPU_DEVICE.fb_bpp as u8),
                ),
            });
        }

        let width = VIRTIO_GPU_DEVICE.display_width;
        let height = VIRTIO_GPU_DEVICE.display_height;
        if width == 0 || height == 0 {
            return None;
        }

        let pitch = width.saturating_mul(4);
        let size = (pitch as u64).saturating_mul(height as u64);
        if size == 0 {
            return None;
        }

        let size_aligned = align_up(size as usize, PAGE_SIZE_4KB as usize) as u64;
        let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
        let phys = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
        if phys.is_null() {
            return None;
        }

        let resource_id = if VIRTIO_GPU_DEVICE.fb_resource_id != 0 {
            VIRTIO_GPU_DEVICE.fb_resource_id
        } else {
            1
        };

        let mmio = virtio_gpu_mmio_caps()?;

        if !virtio_gpu_resource_create_2d(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height)
        {
            free_page_frame(phys);
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
            return None;
        }

        if !virtio_gpu_set_scanout(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height) {
            free_page_frame(phys);
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

        Some(FramebufferData {
            address: phys.to_virt().as_mut_ptr::<u8>(),
            info: DisplayInfo::new(width, height, pitch, PixelFormat::Argb8888),
        })
    }
}

pub fn virtio_gpu_flush_full() -> c_int {
    unsafe {
        // Performance counters - reset but don't log (serial too slow for per-frame output)
        let _fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
        let _spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
        let _completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
        // NOTE: Logging removed - serial output on every frame caused line-by-line rendering
        // To debug performance, use klog_debug! with boot.debug=on (but expect slowdown)

        if VIRTIO_GPU_DEVICE.fb_ready == 0 {
            return -1;
        }
        let width = VIRTIO_GPU_DEVICE.fb_width;
        let height = VIRTIO_GPU_DEVICE.fb_height;
        let resource_id = VIRTIO_GPU_DEVICE.fb_resource_id;
        if resource_id == 0 || width == 0 || height == 0 {
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
            return -1;
        }
        if !virtio_gpu_resource_flush(&mut VIRTIO_GPU_DEVICE, &mmio, resource_id, width, height) {
            return -1;
        }

        0
    }
}
