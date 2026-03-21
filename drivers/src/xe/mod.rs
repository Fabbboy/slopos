#![allow(unsafe_op_in_unsafe_fn)]

use slopos_abi::{DisplayInfo, FramebufferData, PhysAddr, PixelFormat};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_sync::{InitFlag, IrqMutex};
use slopos_utils::{align_up_u64, klog_info, klog_warn};

use crate::pci::{PciDeviceInfo, PciGpuInfo, pci_get_primary_gpu};
use crate::pci_defs::PCI_CLASS_DISPLAY;

mod display;
mod forcewake;
mod ggtt;
mod regs;

const PCI_VENDOR_INTEL: u16 = 0x8086;

#[derive(Copy, Clone)]
#[allow(dead_code)]
struct XeDevice {
    present: bool,
    device: PciDeviceInfo,
    mmio: MmioRegion,
    mmio_size: u64,
    gmd_id: u32,
    ggtt: ggtt::XeGgtt,
    ggtt_ready: bool,
    fb: XeFramebuffer,
}

impl XeDevice {
    const fn empty() -> Self {
        Self {
            present: false,
            device: PciDeviceInfo::zeroed(),
            mmio: MmioRegion::empty(),
            mmio_size: 0,
            gmd_id: 0,
            ggtt: ggtt::XeGgtt::empty(),
            ggtt_ready: false,
            fb: XeFramebuffer::empty(),
        }
    }
}

#[derive(Copy, Clone)]
#[allow(dead_code)]
struct XeFramebuffer {
    ready: bool,
    phys: PhysAddr,
    virt: *mut u8,
    ggtt_addr: u64,
    size: u64,
    width: u32,
    height: u32,
    pitch: u32,
    format: PixelFormat,
}

impl XeFramebuffer {
    const fn empty() -> Self {
        Self {
            ready: false,
            phys: PhysAddr::NULL,
            virt: core::ptr::null_mut(),
            ggtt_addr: 0,
            size: 0,
            width: 0,
            height: 0,
            pitch: 0,
            format: PixelFormat::Argb8888,
        }
    }
}

// Safety: Access to this state is synchronized through `XE_DEVICE` IrqMutex.
unsafe impl Send for XeFramebuffer {}

static XE_DEVICE: IrqMutex<XeDevice> = IrqMutex::new(XeDevice::empty());
static XE_PROBED: InitFlag = InitFlag::new();

fn xe_primary_gpu() -> Option<PciGpuInfo> {
    let gpu = pci_get_primary_gpu();
    if gpu.present == 0 {
        return None;
    }
    Some(gpu)
}

pub fn xe_probe() -> bool {
    if !XE_PROBED.claim() {
        return xe_is_ready();
    }

    let Some(gpu) = xe_primary_gpu() else {
        klog_info!("XE: No primary GPU present during probe");
        return false;
    };

    if gpu.device.vendor_id != PCI_VENDOR_INTEL || gpu.device.class_code != PCI_CLASS_DISPLAY {
        klog_info!(
            "XE: Primary GPU is not Intel display class (vid=0x{:04x} class=0x{:02x})",
            gpu.device.vendor_id,
            gpu.device.class_code
        );
        return false;
    }

    let mmio_region = if gpu.mmio_region.is_mapped() {
        gpu.mmio_region
    } else if gpu.mmio_phys_base != 0 && gpu.mmio_size != 0 {
        MmioRegion::map(PhysAddr::new(gpu.mmio_phys_base), gpu.mmio_size as usize)
            .unwrap_or_else(MmioRegion::empty)
    } else {
        MmioRegion::empty()
    };

    if !mmio_region.is_mapped() {
        klog_warn!("XE: GPU MMIO mapping unavailable");
        return false;
    }

    if !forcewake::forcewake_render_on(&mmio_region) {
        klog_warn!("XE: forcewake render domain failed");
        return false;
    }

    let gmd_id = mmio_region.read::<u32>(regs::GMD_ID);
    if gmd_id == u32::MAX {
        klog_warn!("XE: GMD_ID read failed (0xFFFFFFFF)");
        return false;
    }

    let arch = regs::reg_field_get(regs::GMD_ID_ARCH_MASK, gmd_id);
    let rel = regs::reg_field_get(regs::GMD_ID_RELEASE_MASK, gmd_id);
    let rev = regs::reg_field_get(regs::GMD_ID_REVID_MASK, gmd_id);

    {
        let mut dev = XE_DEVICE.lock();
        *dev = XeDevice {
            present: true,
            device: gpu.device,
            mmio: mmio_region,
            mmio_size: gpu.mmio_size,
            gmd_id,
            ggtt: ggtt::XeGgtt::empty(),
            ggtt_ready: false,
            fb: XeFramebuffer::empty(),
        };
    }

    klog_info!(
        "XE: Probe ok (did=0x{:04x}) gmd_id=0x{:08x} arch={} rel={} rev={}",
        gpu.device.device_id,
        gmd_id,
        arch,
        rel,
        rev
    );
    true
}

pub fn xe_is_ready() -> bool {
    XE_DEVICE.lock().present
}

pub fn xe_framebuffer_init(boot_fb: Option<FramebufferData>) -> Option<FramebufferData> {
    if boot_fb.is_none() {
        klog_warn!("XE: No boot framebuffer available");
        return None;
    }

    if !xe_is_ready() {
        klog_warn!("XE: Probe failed; using boot framebuffer fallback");
        return boot_fb;
    }

    let boot = boot_fb.unwrap();
    let width = boot.info.width;
    let height = boot.info.height;
    if width == 0 || height == 0 {
        klog_warn!("XE: Invalid boot framebuffer dimensions");
        return Some(boot);
    }

    let pitch = align_up_u64(width as u64 * 4, regs::PLANE_STRIDE_ALIGN as u64) as u32;
    let size = pitch as u64 * height as u64;
    let size_aligned = align_up_u64(size, PAGE_SIZE_4KB);
    let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
    if pages == 0 {
        klog_warn!("XE: Framebuffer size invalid for allocation");
        return Some(boot);
    }

    let phys = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
    if phys.is_null() {
        klog_warn!("XE: Failed to allocate framebuffer pages");
        return Some(boot);
    }
    let Some(virt) = phys.to_virt_checked() else {
        klog_warn!("XE: Failed to map framebuffer pages into HHDM");
        let _ = free_page_frame(phys);
        return Some(boot);
    };

    let (mmio, ggtt_addr) = {
        let mut dev = XE_DEVICE.lock();
        let mmio = dev.mmio;

        if !dev.ggtt_ready {
            let Some(ggtt) = ggtt::xe_ggtt_init(&mmio) else {
                klog_warn!("XE: GGTT init failed");
                let _ = free_page_frame(phys);
                return Some(boot);
            };
            dev.ggtt = ggtt;
            dev.ggtt_ready = true;
        }

        let Some(start_entry) = ggtt::xe_ggtt_alloc(&mut dev.ggtt, pages, 16) else {
            klog_warn!("XE: GGTT allocation failed");
            let _ = free_page_frame(phys);
            return Some(boot);
        };

        if !ggtt::xe_ggtt_map(&dev.ggtt, start_entry, phys, pages) {
            klog_warn!("XE: GGTT mapping failed");
            let _ = free_page_frame(phys);
            return Some(boot);
        }

        (mmio, start_entry as u64 * PAGE_SIZE_4KB)
    };

    if !display::xe_display_program_primary(&mmio, ggtt_addr, width, height, pitch) {
        klog_warn!("XE: Display plane programming failed");
        let _ = free_page_frame(phys);
        return Some(boot);
    }

    {
        let mut dev = XE_DEVICE.lock();
        dev.fb = XeFramebuffer {
            ready: true,
            phys,
            virt: virt.as_mut_ptr::<u8>(),
            ggtt_addr,
            size,
            width,
            height,
            pitch,
            format: PixelFormat::Xrgb8888,
        };
    }

    Some(FramebufferData {
        address: virt.as_mut_ptr::<u8>(),
        info: DisplayInfo::new(width, height, pitch, PixelFormat::Xrgb8888),
    })
}

pub fn xe_flush() -> i32 {
    let (present, ready, mmio, ggtt_addr) = {
        let dev = XE_DEVICE.lock();
        (dev.present, dev.fb.ready, dev.mmio, dev.fb.ggtt_addr)
    };
    if !present || !ready {
        return -1;
    }
    if display::xe_display_flush(&mmio, ggtt_addr) {
        0
    } else {
        -1
    }
}
