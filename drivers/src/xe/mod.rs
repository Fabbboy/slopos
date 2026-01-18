#![allow(unsafe_op_in_unsafe_fn)]

use slopos_abi::{FramebufferData, PhysAddr};
use slopos_core::wl_currency::{award_loss, award_win};
use slopos_lib::{InitFlag, klog_info, klog_warn};
use slopos_mm::mmio::MmioRegion;

use crate::pci::{PciDeviceInfo, PciGpuInfo, pci_get_primary_gpu};

mod forcewake;
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
}

impl XeDevice {
    const fn empty() -> Self {
        Self {
            present: false,
            device: PciDeviceInfo::zeroed(),
            mmio: MmioRegion::empty(),
            mmio_size: 0,
        }
    }
}

static mut XE_DEVICE: XeDevice = XeDevice::empty();
static XE_PROBED: InitFlag = InitFlag::new();

fn xe_primary_gpu() -> Option<&'static PciGpuInfo> {
    let gpu = pci_get_primary_gpu();
    if gpu.is_null() {
        return None;
    }
    let info = unsafe { &*gpu };
    if info.present == 0 {
        return None;
    }
    Some(info)
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

    // XE display output not yet implemented - continue using boot-provided framebuffer.
    klog_info!("XE: Using boot framebuffer until XE scanout is wired");
    boot_fb
}

pub fn xe_flush() -> i32 {
    // No-op until XE scanout/submit path is implemented.
    0
}
