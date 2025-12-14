#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::{c_int, c_void};

use slopos_lib::{klog_debug, klog_info};

use crate::pci::{
    pci_bar_info_t, pci_config_read16, pci_config_read8, pci_config_write16, pci_config_write8,
    pci_device_info_t, pci_register_driver, pci_driver_t, PCI_COMMAND_OFFSET,
};
use crate::wl_currency;

unsafe extern "C" {
    fn mm_map_mmio_region(base: u64, size: usize) -> *mut core::ffi::c_void;
    fn mm_unmap_mmio_region(ptr: *mut core::ffi::c_void, size: usize);
}

pub const VIRTIO_GPU_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_GPU_DEVICE_ID_PRIMARY: u16 = 0x1050;
pub const VIRTIO_GPU_DEVICE_ID_TRANS: u16 = 0x1010;

const VIRTIO_PCI_STATUS_OFFSET: u8 = 0x12;
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
const VIRTIO_STATUS_DRIVER: u8 = 0x02;

const VIRTIO_MMIO_DEFAULT_SIZE: usize = 0x1000;
const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;
const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct virtio_gpu_device_t {
    pub present: c_int,
    pub device: pci_device_info_t,
    pub mmio_base: *mut core::ffi::c_void,
    pub mmio_size: usize,
}

static mut VIRTIO_GPU_DEVICE: virtio_gpu_device_t = virtio_gpu_device_t {
    present: 0,
    device: pci_device_info_t::zeroed(),
    mmio_base: core::ptr::null_mut(),
    mmio_size: 0,
};

fn virtio_gpu_enable_master(info: &pci_device_info_t) {
    let command =
        pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let desired = command | PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER;
    if command != desired {
        pci_config_write16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET, desired);
    }
}

extern "C" fn virtio_gpu_match(info: *const pci_device_info_t, _context: *mut c_void) -> bool {
    let info = unsafe { &*info };
    if info.vendor_id != VIRTIO_GPU_VENDOR_ID {
        return false;
    }
    info.device_id == VIRTIO_GPU_DEVICE_ID_PRIMARY || info.device_id == VIRTIO_GPU_DEVICE_ID_TRANS
}

extern "C" fn virtio_gpu_probe(info: *const pci_device_info_t, _context: *mut c_void) -> c_int {
    let info = unsafe { &*info };
    unsafe {
        if VIRTIO_GPU_DEVICE.present != 0 {
            klog_debug!("PCI: virtio-gpu driver already claimed a device");
            return -1;
        }
    }

    let mut bar_opt: Option<&pci_bar_info_t> = None;
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

    let mmio_size = if bar.size != 0 { bar.size as usize } else { VIRTIO_MMIO_DEFAULT_SIZE };
    let mmio_base = unsafe { mm_map_mmio_region(bar.base, mmio_size) };
    if mmio_base.is_null() {
        klog_info!(
            "PCI: virtio-gpu MMIO mapping failed for phys=0x{:x}",
            bar.base
        );
        wl_currency::award_loss();
        return -1;
    }

    virtio_gpu_enable_master(info);

    let status_before = pci_config_read8(info.bus, info.device, info.function, VIRTIO_PCI_STATUS_OFFSET);
    klog_debug!("PCI: virtio-gpu status read=0x{:02x}", status_before);

    pci_config_write8(info.bus, info.device, info.function, VIRTIO_PCI_STATUS_OFFSET, 0x00);
    let status_zeroed = pci_config_read8(info.bus, info.device, info.function, VIRTIO_PCI_STATUS_OFFSET);
    klog_debug!("PCI: virtio-gpu status after clear=0x{:02x}", status_zeroed);

    let handshake = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
    pci_config_write8(info.bus, info.device, info.function, VIRTIO_PCI_STATUS_OFFSET, handshake);
    let status_handshake = pci_config_read8(info.bus, info.device, info.function, VIRTIO_PCI_STATUS_OFFSET);
    if (status_handshake & handshake) != handshake {
        klog_info!(
            "PCI: virtio-gpu handshake incomplete (status=0x{:02x})",
            status_handshake
        );
        unsafe { mm_unmap_mmio_region(mmio_base, mmio_size) };
        wl_currency::award_loss();
        return -1;
    }

    let sample_value = unsafe { *(mmio_base as *mut u32) };
    klog_debug!(
        "PCI: virtio-gpu MMIO sample value=0x{:08x}",
        sample_value
    );

    unsafe {
        VIRTIO_GPU_DEVICE.present = 1;
        VIRTIO_GPU_DEVICE.device = *info;
        VIRTIO_GPU_DEVICE.mmio_base = mmio_base;
        VIRTIO_GPU_DEVICE.mmio_size = mmio_size;
    }

    klog_info!("PCI: virtio-gpu driver probe succeeded (wheel gave a W)");
    wl_currency::award_win();
    0
}

static VIRTIO_GPU_PCI_DRIVER: pci_driver_t = pci_driver_t {
    name: b"virtio-gpu\0".as_ptr(),
    match_fn: Some(virtio_gpu_match),
    probe: Some(virtio_gpu_probe),
    context: core::ptr::null_mut(),
};

#[unsafe(no_mangle)]
pub extern "C" fn virtio_gpu_register_driver() {
    static mut REGISTERED: bool = false;
    unsafe {
        if REGISTERED {
            return;
        }
        if pci_register_driver(&VIRTIO_GPU_PCI_DRIVER as *const pci_driver_t) != 0 {
            klog_info!("PCI: virtio-gpu driver registration failed");
        }
        REGISTERED = true;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn virtio_gpu_get_device() -> *const virtio_gpu_device_t {
    unsafe {
        if VIRTIO_GPU_DEVICE.present != 0 {
            &VIRTIO_GPU_DEVICE as *const virtio_gpu_device_t
        } else {
            core::ptr::null()
        }
    }
}
