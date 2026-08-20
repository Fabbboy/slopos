//! VirtIO PCI capability parsing, device initialization, and MSI-X/MSI setup

use crate::driver_core::bound::BoundDevice;
use crate::driver_core::msi::{self as core_msi, IrqMechanism};
use crate::msix;
use crate::pci_defs::{PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET};
use slopos_abi::addr::PhysAddr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::klog_info;

use crate::pci::{
    PciDeviceInfo, pci_config_read8, pci_config_read16, pci_config_read32, pci_config_write16,
};

use super::{
    COMMON_CFG_DEVICE_FEATURE, COMMON_CFG_DEVICE_FEATURE_SELECT, COMMON_CFG_DRIVER_FEATURE,
    COMMON_CFG_DRIVER_FEATURE_SELECT, COMMON_CFG_MSIX_CONFIG, InterruptMode, MAX_MSIX_QUEUES,
    PCI_CAP_ID_VNDR, PCI_CAP_PTR_OFFSET, PCI_STATUS_CAP_LIST, PCI_STATUS_OFFSET,
    VIRTIO_MSI_NO_VECTOR, VIRTIO_PCI_CAP_COMMON_CFG, VIRTIO_PCI_CAP_DEVICE_CFG,
    VIRTIO_PCI_CAP_NOTIFY_CFG, VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER,
    VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FEATURES_OK, VirtioMmioCaps, VirtioMsixState,
    get_device_status, reset_device, set_device_status,
};

pub use crate::pci_defs::PCI_VENDOR_ID_VIRTIO;

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
    // The device supplies both offset and length; a function that declares a
    // window past its own BAR would otherwise get whatever physical memory
    // sits there mapped for it.
    let Some(base) = bar_info.window(offset as u64, length as u64) else {
        klog_info!(
            "virtio: capability window bar{} +{:#x}..{:#x} escapes BAR size {:#x}; ignoring",
            bar,
            offset,
            offset as u64 + length as u64,
            bar_info.size,
        );
        return MmioRegion::empty();
    };
    MmioRegion::map(PhysAddr::new(base), length as usize).unwrap_or_else(MmioRegion::empty)
}

pub fn parse_capabilities(info: &PciDeviceInfo) -> VirtioMmioCaps {
    let mut caps = VirtioMmioCaps::empty();

    let status = pci_config_read16(info.bus, info.device, info.function, PCI_STATUS_OFFSET);
    if (status & PCI_STATUS_CAP_LIST) == 0 {
        return caps;
    }

    let mut cap_ptr =
        (pci_config_read8(info.bus, info.device, info.function, PCI_CAP_PTR_OFFSET) & 0xFC) as u16;
    let mut guard = 0u8;

    while cap_ptr != 0 && guard < 48 {
        guard += 1;

        let cap_id = pci_config_read8(info.bus, info.device, info.function, cap_ptr);
        let cap_next =
            (pci_config_read8(info.bus, info.device, info.function, cap_ptr + 1) & 0xFC) as u16;
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

    cfg.write::<u32>(COMMON_CFG_DEVICE_FEATURE_SELECT, 0);
    let features_lo = cfg.read::<u32>(COMMON_CFG_DEVICE_FEATURE) as u64;
    cfg.write::<u32>(COMMON_CFG_DEVICE_FEATURE_SELECT, 1);
    let features_hi = cfg.read::<u32>(COMMON_CFG_DEVICE_FEATURE) as u64;
    let device_features = features_lo | (features_hi << 32);

    let driver_features = device_features & (required_features | optional_features);

    cfg.write::<u32>(COMMON_CFG_DRIVER_FEATURE_SELECT, 0);
    cfg.write::<u32>(COMMON_CFG_DRIVER_FEATURE, driver_features as u32);
    cfg.write::<u32>(COMMON_CFG_DRIVER_FEATURE_SELECT, 1);
    cfg.write::<u32>(COMMON_CFG_DRIVER_FEATURE, (driver_features >> 32) as u32);

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

/// Must be called **after** feature negotiation and **before**
/// [`set_driver_ok`]; the returned vectors are written into the queues during
/// queue setup, which must also precede `DRIVER_OK`. The config-change MSI-X
/// entry is intentionally left at [`VIRTIO_MSI_NO_VECTOR`].
///
/// `Err` means the device has neither MSI-X nor MSI, which on QEMU q35 points at
/// a configuration or hardware problem rather than an unsupported device.
pub fn setup_interrupts<H: Fn(u8) + Clone + Send + Sync + 'static>(
    bound: &mut BoundDevice<'_>,
    caps: &VirtioMmioCaps,
    num_queues: u8,
    handler: H,
) -> Result<(InterruptMode, Option<VirtioMsixState>), &'static str> {
    let info = *bound.info();
    let nq = (num_queues as usize).min(MAX_MSIX_QUEUES);
    if nq == 0 {
        return Err("virtio: zero queues requested for interrupt setup");
    }

    let mut queue_vectors = [0u8; MAX_MSIX_QUEUES];
    match core_msi::setup_interrupts(bound, nq, &mut queue_vectors, handler) {
        Some(IrqMechanism::Msix { cap, table }) => {
            if caps.has_common_cfg() {
                caps.common_cfg
                    .write::<u16>(COMMON_CFG_MSIX_CONFIG, VIRTIO_MSI_NO_VECTOR);
            }
            msix::msix_enable(info.bus, info.device, info.function, &cap);
            klog_info!(
                "virtio-msix: {}:{}.{} enabled, {} queue vectors",
                info.bus,
                info.device,
                info.function,
                nq,
            );
            Ok((
                InterruptMode::Msix {
                    num_queues: nq as u8,
                },
                Some(VirtioMsixState {
                    cap,
                    table,
                    queue_vectors,
                    num_queues: nq as u8,
                }),
            ))
        }
        // MSI is enabled inside `msi_configure`; nothing virtio-specific to add.
        Some(IrqMechanism::Msi { vector, .. }) => {
            klog_info!(
                "virtio-msi: {}:{}.{} enabled, vector 0x{:02x}",
                info.bus,
                info.device,
                info.function,
                vector,
            );
            Ok((InterruptMode::Msi { vector }, None))
        }
        None => Err("virtio: device has neither MSI-X nor MSI — cannot configure interrupts"),
    }
}
