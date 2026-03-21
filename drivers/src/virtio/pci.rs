//! VirtIO PCI capability parsing, device initialization, and MSI-X/MSI setup

use crate::msi::{self, MsiCapability};
use crate::msix;
use crate::pci_defs::{PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET};
use slopos_abi::addr::PhysAddr;
use slopos_mm::mmio::MmioRegion;
use slopos_utils::{klog_debug, klog_info};

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

// =============================================================================
// MSI-X / MSI Interrupt Setup
// =============================================================================

/// Attempt to set up MSI-X for a VirtIO device.
///
/// Allocates one IDT vector per queue and programs the MSI-X table so each
/// virtqueue fires its own interrupt.  The caller passes the allocated vectors
/// to [`queue::setup_queue`] via the `msix_vector` parameter.
///
/// The config-change MSI-X entry is intentionally not configured (set to
/// [`VIRTIO_MSI_NO_VECTOR`]): config changes are rare and the polling drivers
/// do not depend on them.
///
/// # VirtIO initialisation ordering
///
/// This function must be called **after** feature negotiation and **before**
/// [`set_driver_ok`].  The returned vectors are written to
/// `queue_msix_vector` during [`queue::setup_queue`], which must also happen
/// before `DRIVER_OK`.
///
/// Returns `None` if the device has no MSI-X capability, the table cannot be
/// mapped, or vector allocation fails.  Partial allocations are cleaned up
/// automatically.
pub fn try_setup_msix(
    info: &PciDeviceInfo,
    caps: &VirtioMmioCaps,
    num_queues: u8,
) -> Option<VirtioMsixState> {
    let cap_offset = info.msix_cap_offset?;
    let nq = (num_queues as usize).min(MAX_MSIX_QUEUES);
    if nq == 0 {
        return None;
    }

    // 1. Parse MSI-X capability.
    let cap = msix::msix_read_capability(info.bus, info.device, info.function, cap_offset);
    if (cap.table_size as usize) < nq {
        klog_debug!(
            "virtio-msix: device {}:{}.{} has only {} MSI-X entries, need {}",
            info.bus,
            info.device,
            info.function,
            cap.table_size,
            nq,
        );
        return None;
    }

    // 2. Map the MSI-X table + PBA.
    let table = match msix::msix_map_table(info, &cap) {
        Ok(t) => t,
        Err(e) => {
            klog_debug!("virtio-msix: table map failed: {:?}", e);
            return None;
        }
    };

    // 3. Allocate IDT vectors and program table entries.
    let apic_id: u8 = 0; // target BSP
    let mut queue_vectors = [0u8; MAX_MSIX_QUEUES];
    for i in 0..nq {
        let vector = match slopos_core::irq::msi_alloc_vector() {
            Some(v) => v,
            None => {
                // Roll back previously allocated vectors.
                for v in &queue_vectors[..i] {
                    if *v != 0 {
                        slopos_core::irq::msi_free_vector(*v);
                    }
                }
                klog_debug!("virtio-msix: vector allocation exhausted at queue {}", i);
                return None;
            }
        };

        if let Err(e) = msix::msix_configure(&table, i as u16, vector, apic_id) {
            slopos_core::irq::msi_free_vector(vector);
            for v in &queue_vectors[..i] {
                if *v != 0 {
                    slopos_core::irq::msi_free_vector(*v);
                }
            }
            klog_debug!("virtio-msix: configure entry {} failed: {:?}", i, e);
            return None;
        }

        queue_vectors[i] = vector;
    }

    // 4. Tell the device we are NOT using a config-change MSI-X vector.
    if caps.has_common_cfg() {
        caps.common_cfg
            .write::<u16>(COMMON_CFG_MSIX_CONFIG, VIRTIO_MSI_NO_VECTOR);
    }

    // 5. Enable MSI-X on the PCI function.
    msix::msix_enable(info.bus, info.device, info.function, &cap);

    klog_info!(
        "virtio-msix: {}:{}.{} enabled, {} queue vectors",
        info.bus,
        info.device,
        info.function,
        nq,
    );

    Some(VirtioMsixState {
        cap,
        table,
        queue_vectors,
        num_queues: nq as u8,
    })
}

/// Attempt to set up MSI (non-X) for a VirtIO device.
///
/// Allocates a single IDT vector shared across all queues.  MSI-X should be
/// preferred when available; this is the fallback.
///
/// Returns the allocated vector, or `None` if the device has no MSI capability
/// or vector allocation fails.
pub fn try_setup_msi(info: &PciDeviceInfo) -> Option<(MsiCapability, u8)> {
    let cap_offset = info.msi_cap_offset?;
    let cap = msi::msi_read_capability(info.bus, info.device, info.function, cap_offset);

    let vector = slopos_core::irq::msi_alloc_vector()?;
    if let Err(_e) = msi::msi_configure(
        info.bus,
        info.device,
        info.function,
        &cap,
        vector,
        0, // target BSP
    ) {
        slopos_core::irq::msi_free_vector(vector);
        return None;
    }

    klog_info!(
        "virtio-msi: {}:{}.{} enabled, vector 0x{:02x}",
        info.bus,
        info.device,
        info.function,
        vector,
    );

    Some((cap, vector))
}

/// Set up the best available interrupt mechanism for a VirtIO device.
///
/// Tries MSI-X first (per-queue vectors), then MSI (single shared vector).
/// Returns `Err` if neither mechanism is available — VirtIO modern devices
/// on QEMU q35 always have MSI-X, so this indicates a configuration or
/// hardware problem.
pub fn setup_interrupts(
    info: &PciDeviceInfo,
    caps: &VirtioMmioCaps,
    num_queues: u8,
) -> Result<(InterruptMode, Option<VirtioMsixState>), &'static str> {
    // Prefer MSI-X — per-queue vectors.
    if let Some(msix_state) = try_setup_msix(info, caps, num_queues) {
        return Ok((
            InterruptMode::Msix {
                num_queues: msix_state.num_queues,
            },
            Some(msix_state),
        ));
    }

    // Fallback: MSI — single shared vector.
    if let Some((_cap, vector)) = try_setup_msi(info) {
        return Ok((InterruptMode::Msi { vector }, None));
    }

    Err("virtio: device has neither MSI-X nor MSI — cannot configure interrupts")
}

/// Register MSI-X or MSI interrupt handlers for a VirtIO device.
///
/// `handler` is invoked by the IRQ dispatch layer when the device fires an
/// interrupt.  For MSI-X, a handler is registered for each per-queue vector.
/// For MSI, a single handler is registered for the shared vector.
///
/// `device_bdf` is `(bus << 16) | (dev << 8) | func`.
pub fn register_irq_handlers(
    mode: &InterruptMode,
    msix_state: Option<&VirtioMsixState>,
    handler: slopos_core::irq::MsiHandler,
    device_bdf: u32,
) {
    match mode {
        InterruptMode::Msix { num_queues } => {
            if let Some(state) = msix_state {
                for i in 0..*num_queues as usize {
                    let vec = state.queue_vectors[i];
                    if vec != 0 {
                        slopos_core::irq::msi_register_handler(
                            vec,
                            handler,
                            i as *mut core::ffi::c_void,
                            device_bdf,
                        );
                    }
                }
            }
        }
        InterruptMode::Msi { vector } => {
            slopos_core::irq::msi_register_handler(
                *vector,
                handler,
                core::ptr::null_mut(),
                device_bdf,
            );
        }
    }
}
