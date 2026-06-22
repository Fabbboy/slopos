#![allow(unsafe_op_in_unsafe_fn)]

use slopos_abi::{DisplayInfo, FramebufferData, PhysAddr, PixelFormat};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_mm::page_alloc::{alloc_kernel_pages, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{align_up_u64, klog_info, klog_warn};

use slopos_kernel_services::syscall_services::scanout::{
    self, ClaimOutcome, InstallCtx, ScanoutId, ScanoutProvider,
};

use crate::pci::{PciDeviceInfo, PciProbeError};
use crate::pci_defs::PCI_CLASS_DISPLAY;

mod display;
mod forcewake;
mod ggtt;
mod regs;

const PCI_VENDOR_INTEL: u16 = 0x8086;

#[derive(Clone)]
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
    /// HHDM virtual address of the backing, as an integer so `XeDevice` stays
    /// `Send` (a raw pointer would make the `XE_DEVICE` static non-`Sync`).
    virt: u64,
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
            virt: 0,
            ggtt_addr: 0,
            size: 0,
            width: 0,
            height: 0,
            pitch: 0,
            format: PixelFormat::Argb8888,
        }
    }
}

// Safety: Access to this state is synchronized through `XE_DEVICE` SpinLock.

static XE_DEVICE: SpinLock<XeDevice> = SpinLock::new(XeDevice::empty(), LOCK_LEVEL_RESOURCE);

const fn xe_matches(info: &PciDeviceInfo) -> bool {
    info.vendor_id == PCI_VENDOR_INTEL && info.class_code == PCI_CLASS_DISPLAY
}

/// First memory-mapped BAR (Intel GTTMMADR is BAR0): the GPU register window.
fn xe_mmio_bar(info: &PciDeviceInfo) -> Option<(u64, u64)> {
    for bar in &info.bars {
        if bar.is_io == 0 && bar.base != 0 && bar.size != 0 {
            return Some((bar.base, bar.size));
        }
    }
    None
}

/// Eviction hook: GPU→GPU re-claim is deferred, so a displaced xe has nothing
/// to do here yet.
fn xe_evict() {}

fn xe_probe(info: &PciDeviceInfo) -> Result<(), PciProbeError> {
    // Reserve the scanout before touching the device. If a higher-priority
    // provider already owns it, stay passive and touch nothing.
    match scanout::SCANOUT.claim(scanout::PRIO_INTEL_XE) {
        ClaimOutcome::Won => {}
        ClaimOutcome::Lost | ClaimOutcome::LostTie => {
            klog_info!("XE: lost scanout arbitration; staying passive");
            return Ok(());
        }
    }

    if let Err(err) = xe_bring_up(info) {
        scanout::SCANOUT.abort_claim();
        return Err(err);
    }

    // Allocate the xe-owned scanout, sized to match the current firmware
    // framebuffer, and program the display plane.
    let Some(xe_fb) = scanout::current_framebuffer().and_then(xe_setup_framebuffer) else {
        klog_warn!("XE: framebuffer setup failed");
        scanout::SCANOUT.abort_claim();
        return Err(PciProbeError::DeviceFault);
    };

    scanout::SCANOUT.commit_install(
        ScanoutProvider {
            id: ScanoutId::IntelXe,
            priority: scanout::PRIO_INTEL_XE,
            evict: xe_evict,
        },
        scanout::PRIO_INTEL_XE,
        |displaced| {
            if let Some(p) = displaced {
                (p.evict)();
            }
        },
    );

    let ctx = InstallCtx {
        fb: xe_fb,
        flush: Some(xe_flush),
        gpu_control: None,
    };
    if !scanout::run_scanout_install(&ctx) {
        klog_warn!("XE: scanout install failed");
        return Err(PciProbeError::DeviceFault);
    }
    Ok(())
}

/// Map the GPU MMIO window, enable forcewake, identify the GPU, and publish it.
fn xe_bring_up(info: &PciDeviceInfo) -> Result<(), PciProbeError> {
    let Some((mmio_phys, mmio_size)) = xe_mmio_bar(info) else {
        klog_warn!("XE: no MMIO BAR present");
        return Err(PciProbeError::Unsupported);
    };

    let mmio_region = MmioRegion::map(PhysAddr::new(mmio_phys), mmio_size as usize)
        .unwrap_or_else(MmioRegion::empty);
    if !mmio_region.is_mapped() {
        klog_warn!("XE: GPU MMIO mapping unavailable");
        return Err(PciProbeError::DeviceFault);
    }

    if !forcewake::forcewake_render_on(&mmio_region) {
        klog_warn!("XE: forcewake render domain failed");
        return Err(PciProbeError::DeviceFault);
    }

    let gmd_id = mmio_region.read::<u32>(regs::GMD_ID);
    if gmd_id == u32::MAX {
        klog_warn!("XE: GMD_ID read failed (0xFFFFFFFF)");
        return Err(PciProbeError::DeviceFault);
    }

    let arch = regs::reg_field_get(regs::GMD_ID_ARCH_MASK, gmd_id);
    let rel = regs::reg_field_get(regs::GMD_ID_RELEASE_MASK, gmd_id);
    let rev = regs::reg_field_get(regs::GMD_ID_REVID_MASK, gmd_id);

    {
        let mut dev = XE_DEVICE.lock();
        *dev = XeDevice {
            present: true,
            device: *info,
            mmio: mmio_region,
            mmio_size,
            gmd_id,
            ggtt: ggtt::XeGgtt::empty(),
            ggtt_ready: false,
            fb: XeFramebuffer::empty(),
        };
    }

    klog_info!(
        "XE: Probe ok (did=0x{:04x}) gmd_id=0x{:08x} arch={} rel={} rev={}",
        info.device_id,
        gmd_id,
        arch,
        rel,
        rev
    );
    Ok(())
}

/// Allocate the xe-owned framebuffer, map it through the GGTT, and program the
/// primary display plane, sized to match `seed`. Returns the GPU-visible
/// framebuffer, or `None` on any failure (the caller aborts the scanout claim).
fn xe_setup_framebuffer(seed: FramebufferData) -> Option<FramebufferData> {
    let width = seed.info.width;
    let height = seed.info.height;
    if width == 0 || height == 0 {
        klog_warn!("XE: Invalid seed framebuffer dimensions");
        return None;
    }

    let pitch = align_up_u64(width as u64 * 4, regs::PLANE_STRIDE_ALIGN as u64) as u32;
    let size = pitch as u64 * height as u64;
    let size_aligned = align_up_u64(size, PAGE_SIZE_4KB);
    let pages = (size_aligned / PAGE_SIZE_4KB) as u32;
    if pages == 0 {
        klog_warn!("XE: Framebuffer size invalid for allocation");
        return None;
    }

    let phys = alloc_kernel_pages(pages);
    if phys.is_null() {
        klog_warn!("XE: Failed to allocate framebuffer pages");
        return None;
    }
    let Some(virt) = phys.to_virt_checked() else {
        klog_warn!("XE: Failed to map framebuffer pages into HHDM");
        let _ = free_page_frame(phys);
        return None;
    };

    let (mmio, ggtt_addr) = {
        let mut dev = XE_DEVICE.lock();
        let mmio = dev.mmio.clone();

        if !dev.ggtt_ready {
            let Some(ggtt) = ggtt::xe_ggtt_init(&mmio) else {
                klog_warn!("XE: GGTT init failed");
                let _ = free_page_frame(phys);
                return None;
            };
            dev.ggtt = ggtt;
            dev.ggtt_ready = true;
        }

        let Some(start_entry) = ggtt::xe_ggtt_alloc(&mut dev.ggtt, pages, 16) else {
            klog_warn!("XE: GGTT allocation failed");
            let _ = free_page_frame(phys);
            return None;
        };

        if !ggtt::xe_ggtt_map(&dev.ggtt, start_entry, phys, pages) {
            klog_warn!("XE: GGTT mapping failed");
            let _ = free_page_frame(phys);
            return None;
        }

        (mmio, start_entry as u64 * PAGE_SIZE_4KB)
    };

    if !display::xe_display_program_primary(&mmio, ggtt_addr, width, height, pitch) {
        klog_warn!("XE: Display plane programming failed");
        let _ = free_page_frame(phys);
        return None;
    }

    {
        let mut dev = XE_DEVICE.lock();
        dev.fb = XeFramebuffer {
            ready: true,
            phys,
            virt: virt.as_u64(),
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

/// Framebuffer flush callback. Xe scans out the kernel framebuffer directly
/// (it *is* the GPU-mapped surface), so a present is just re-arming the plane
/// surface address — the damage region is irrelevant and ignored.
pub fn xe_flush(_damage: *const slopos_abi::damage::DamageRect, _count: u32) -> i32 {
    let (present, ready, mmio, ggtt_addr) = {
        let dev = XE_DEVICE.lock();
        (
            dev.present,
            dev.fb.ready,
            dev.mmio.clone(),
            dev.fb.ggtt_addr,
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

crate::pci_driver! {
    pub static XE_DRIVER = {
        name: "intel-xe",
        matches: xe_matches,
        probe: xe_probe,
    };
}
