#![allow(unsafe_op_in_unsafe_fn)]

use slopos_abi::{DisplayInfo, FramebufferData, PhysAddr, PixelFormat};
use slopos_core::wl_currency::{award_loss, award_win};
use slopos_lib::{InitFlag, align_up_u64, klog_info, klog_warn};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};

use crate::pci::{PciDeviceInfo, PciGpuInfo, pci_get_primary_gpu};

mod display;
mod forcewake;
mod ggtt;
mod mmio;
mod regs;

const PCI_VENDOR_INTEL: u16 = 0x8086;
const PCI_CLASS_DISPLAY: u8 = 0x03;

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

static mut XE_DEVICE: XeDevice = XeDevice::empty();
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
        // Recoverable: no GPU detected when XE was requested.
        award_loss();
        return false;
    };

    if gpu.device.vendor_id != PCI_VENDOR_INTEL || gpu.device.class_code != PCI_CLASS_DISPLAY {
        klog_info!(
            "XE: Primary GPU is not Intel display class (vid=0x{:04x} class=0x{:02x})",
            gpu.device.vendor_id,
            gpu.device.class_code
        );
        // Recoverable: non-Intel or non-display device.
        award_loss();
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
        // Recoverable: cannot access registers, fallback to boot framebuffer.
        award_loss();
        return false;
    }

    if !forcewake::forcewake_render_on(&mmio_region) {
        klog_warn!("XE: forcewake render domain failed");
        // Recoverable: keep boot framebuffer path alive.
        award_loss();
        return false;
    }

    let gmd_id = mmio::read32(&mmio_region, regs::GMD_ID);
    if gmd_id == u32::MAX {
        klog_warn!("XE: GMD_ID read failed (0xFFFFFFFF)");
        award_loss();
        return false;
    }

    let arch = regs::reg_field_get(regs::GMD_ID_ARCH_MASK, gmd_id);
    let rel = regs::reg_field_get(regs::GMD_ID_RELEASE_MASK, gmd_id);
    let rev = regs::reg_field_get(regs::GMD_ID_REVID_MASK, gmd_id);

    unsafe {
        XE_DEVICE = XeDevice {
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
    // Successful probe: award a win for the Wheel of Fate.
    award_win();
    true
}

pub fn xe_is_ready() -> bool {
    unsafe { XE_DEVICE.present }
}

pub fn xe_framebuffer_init(boot_fb: Option<FramebufferData>) -> Option<FramebufferData> {
    if boot_fb.is_none() {
        klog_warn!("XE: No boot framebuffer available");
        // Recoverable: no framebuffer for scanout.
        award_loss();
        return None;
    }

    if !xe_is_ready() {
        klog_warn!("XE: Probe failed; using boot framebuffer fallback");
        // Recoverable: fallback keeps rendering alive.
        award_loss();
        return boot_fb;
    }

    let boot = boot_fb.unwrap();
    let width = boot.info.width;
    let height = boot.info.height;
    if width == 0 || height == 0 {
        klog_warn!("XE: Invalid boot framebuffer dimensions");
        award_loss();
        return Some(boot);
    }

    let pitch = align_up_u64(width as u64 * 4, regs::PLANE_STRIDE_ALIGN as u64) as u32;
    let size = pitch as u64 * height as u64;
    let size_aligned = align_up_u64(size, PAGE_SIZE_4KB);
    let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
    if pages == 0 {
        klog_warn!("XE: Framebuffer size invalid for allocation");
        award_loss();
        return Some(boot);
    }

    let phys = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
    if phys.is_null() {
        klog_warn!("XE: Failed to allocate framebuffer pages");
        award_loss();
        return Some(boot);
    }
    let Some(virt) = phys.to_virt_checked() else {
        klog_warn!("XE: Failed to map framebuffer pages into HHDM");
        let _ = free_page_frame(phys);
        award_loss();
        return Some(boot);
    };

    let mmio = unsafe { XE_DEVICE.mmio };
    let ggtt_addr = unsafe {
        if !XE_DEVICE.ggtt_ready {
            let Some(ggtt) = ggtt::xe_ggtt_init(&mmio) else {
                klog_warn!("XE: GGTT init failed");
                let _ = free_page_frame(phys);
                award_loss();
                return Some(boot);
            };
            core::ptr::addr_of_mut!(XE_DEVICE.ggtt).write(ggtt);
            XE_DEVICE.ggtt_ready = true;
        }

        let ggtt_ptr = core::ptr::addr_of_mut!(XE_DEVICE.ggtt);
        let Some(start_entry) = ggtt::xe_ggtt_alloc(&mut *ggtt_ptr, pages, 16) else {
            klog_warn!("XE: GGTT allocation failed");
            let _ = free_page_frame(phys);
            award_loss();
            return Some(boot);
        };

        if !ggtt::xe_ggtt_map(&*ggtt_ptr, start_entry, phys, pages) {
            klog_warn!("XE: GGTT mapping failed");
            let _ = free_page_frame(phys);
            award_loss();
            return Some(boot);
        }

        start_entry as u64 * PAGE_SIZE_4KB
    };

    if !display::xe_display_program_primary(&mmio, ggtt_addr, width, height, pitch) {
        klog_warn!("XE: Display plane programming failed");
        let _ = free_page_frame(phys);
        award_loss();
        return Some(boot);
    }

    unsafe {
        XE_DEVICE.fb = XeFramebuffer {
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

    award_win();
    Some(FramebufferData {
        address: virt.as_mut_ptr::<u8>(),
        info: DisplayInfo::new(width, height, pitch, PixelFormat::Xrgb8888),
    })
}

pub fn xe_flush() -> i32 {
    let (present, ready, mmio, ggtt_addr) = unsafe {
        (
            XE_DEVICE.present,
            XE_DEVICE.fb.ready,
            XE_DEVICE.mmio,
            XE_DEVICE.fb.ggtt_addr,
        )
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
