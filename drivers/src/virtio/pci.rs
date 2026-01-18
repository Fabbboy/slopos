//! VirtIO PCI capability parsing and device initialization

use slopos_abi::addr::PhysAddr;
use slopos_abi::arch::x86_64::pci::{
    PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET,
};
use slopos_mm::mmio::MmioRegion;

use crate::pci::{
    PciDeviceInfo, pci_config_read8, pci_config_read16, pci_config_read32, pci_config_write16,
};

use super::{
    COMMON_CFG_DEVICE_FEATURE, COMMON_CFG_DEVICE_FEATURE_SELECT, COMMON_CFG_DRIVER_FEATURE,
    COMMON_CFG_DRIVER_FEATURE_SELECT, PCI_CAP_ID_VNDR, PCI_CAP_PTR_OFFSET, PCI_STATUS_CAP_LIST,
    PCI_STATUS_OFFSET, VIRTIO_PCI_CAP_COMMON_CFG, VIRTIO_PCI_CAP_DEVICE_CFG,
    VIRTIO_PCI_CAP_ISR_CFG, VIRTIO_PCI_CAP_NOTIFY_CFG, VIRTIO_STATUS_ACKNOWLEDGE,
    VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FEATURES_OK, VirtioMmioCaps,
    get_device_status, reset_device, set_device_status,
};

pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;

pub fn enable_bus_master(info: &PciDeviceInfo) {
    let cmd = pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let new_cmd = cmd | PCI_COMMAND_BUS_MASTER | PCI_COMMAND_MEMORY_SPACE;
    if cmd != new_cmd {
        pci_config_write16(
            info.bus,
            info.device,
            info.function,
            PCI_COMMAND_OFFSET,
            new_cmd,
        );
    }
}

fn map_cap_region(info: &PciDeviceInfo, bar: u8, offset: u32, length: u32) -> MmioRegion {
    if bar as usize >= info.bars.len() {
        return MmioRegion::empty();
    }
    let bar_info = &info.bars[bar as usize];
    if bar_info.base == 0 || bar_info.is_io != 0 {
        return MmioRegion::empty();
    }
    let phys = PhysAddr::new(bar_info.base.wrapping_add(offset as u64));
    MmioRegion::map(phys, length as usize).unwrap_or_else(MmioRegion::empty)
}

pub fn parse_capabilities(info: &PciDeviceInfo) -> VirtioMmioCaps {
    let mut caps = VirtioMmioCaps::empty();

    let status = pci_config_read16(info.bus, info.device, info.function, PCI_STATUS_OFFSET);
    if (status & PCI_STATUS_CAP_LIST) == 0 {
        return caps;
    }

    let mut cap_ptr = pci_config_read8(info.bus, info.device, info.function, PCI_CAP_PTR_OFFSET);
    let mut guard = 0u8;

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

            let region = map_cap_region(info, bar, offset, length);

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => caps.common_cfg = region,
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    caps.notify_cfg = region;
                    caps.notify_off_multiplier =
                        pci_config_read32(info.bus, info.device, info.function, cap_ptr + 16);
                }
                VIRTIO_PCI_CAP_ISR_CFG => caps.isr_cfg = region,
                VIRTIO_PCI_CAP_DEVICE_CFG => {
                    caps.device_cfg = region;
                    caps.device_cfg_len = length;
                }
                _ => {}
            }
        }

        cap_ptr = cap_next;
    }

    caps
}

pub struct FeatureNegotiation {
    pub device_features: u64,
    pub driver_features: u64,
    pub success: bool,
}

pub fn negotiate_features(
    caps: &VirtioMmioCaps,
    required_features: u64,
    optional_features: u64,
) -> FeatureNegotiation {
    let cfg = &caps.common_cfg;
    if !cfg.is_mapped() {
        return FeatureNegotiation {
            device_features: 0,
            driver_features: 0,
            success: false,
        };
    }

    reset_device(cfg);

    let mut status = get_device_status(cfg);
    status |= VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
    set_device_status(cfg, status);

    cfg.write_u32(COMMON_CFG_DEVICE_FEATURE_SELECT, 0);
    let features_lo = cfg.read_u32(COMMON_CFG_DEVICE_FEATURE) as u64;
    cfg.write_u32(COMMON_CFG_DEVICE_FEATURE_SELECT, 1);
    let features_hi = cfg.read_u32(COMMON_CFG_DEVICE_FEATURE) as u64;
    let device_features = features_lo | (features_hi << 32);

    let driver_features = device_features & (required_features | optional_features);

    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE_SELECT, 0);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE, driver_features as u32);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE_SELECT, 1);
    cfg.write_u32(COMMON_CFG_DRIVER_FEATURE, (driver_features >> 32) as u32);

    status |= VIRTIO_STATUS_FEATURES_OK;
    set_device_status(cfg, status);

    let check = get_device_status(cfg);
    let success = (check & VIRTIO_STATUS_FEATURES_OK) != 0;

    FeatureNegotiation {
        device_features,
        driver_features,
        success,
    }
}

pub fn set_driver_ok(caps: &VirtioMmioCaps) {
    let cfg = &caps.common_cfg;
    if cfg.is_mapped() {
        let mut status = get_device_status(cfg);
        status |= VIRTIO_STATUS_DRIVER_OK;
        set_device_status(cfg, status);
    }
}
