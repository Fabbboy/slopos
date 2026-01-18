//! VirtIO common infrastructure
//!
//! This module provides shared types, constants, and utilities for VirtIO device drivers.
//! It eliminates code duplication between virtio-blk, virtio-gpu, and future virtio drivers.

pub mod pci;
pub mod queue;

use slopos_mm::mmio::MmioRegion;

// =============================================================================
// VirtIO PCI Capability Types
// =============================================================================

/// VirtIO PCI capability type: Common configuration
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 0x01;
/// VirtIO PCI capability type: Notification area
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 0x02;
/// VirtIO PCI capability type: ISR status
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 0x03;
/// VirtIO PCI capability type: Device-specific configuration
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 0x04;

// =============================================================================
// VirtIO Device Status Bits
// =============================================================================

/// Device status: OS has found the device
pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
/// Device status: OS knows how to drive the device
pub const VIRTIO_STATUS_DRIVER: u8 = 0x02;
/// Device status: Driver is ready to drive the device
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 0x04;
/// Device status: Feature negotiation complete
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 0x08;
/// Device status: Something went wrong (device should be reset)
pub const VIRTIO_STATUS_FAILED: u8 = 0x80;

// =============================================================================
// VirtIO Feature Bits
// =============================================================================

/// VirtIO 1.0+ compliant device
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// =============================================================================
// VirtIO Queue Descriptor Flags
// =============================================================================

/// Descriptor continues via the `next` field
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Buffer is device-writable (vs device-readable)
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Buffer contains a list of buffer descriptors
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

// =============================================================================
// PCI Configuration Constants
// =============================================================================

/// PCI Status register offset
pub const PCI_STATUS_OFFSET: u8 = 0x06;
/// PCI Status: Capabilities list present
pub const PCI_STATUS_CAP_LIST: u16 = 0x10;
/// PCI Capabilities pointer offset
pub const PCI_CAP_PTR_OFFSET: u8 = 0x34;
/// PCI Capability ID: Vendor-specific (used by VirtIO)
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

// =============================================================================
// VirtIO Common Configuration Layout (MMIO offsets)
// =============================================================================

/// Offset to device_feature_select in common config
pub const COMMON_CFG_DEVICE_FEATURE_SELECT: usize = 0x00;
/// Offset to device_feature in common config
pub const COMMON_CFG_DEVICE_FEATURE: usize = 0x04;
/// Offset to driver_feature_select in common config
pub const COMMON_CFG_DRIVER_FEATURE_SELECT: usize = 0x08;
/// Offset to driver_feature in common config
pub const COMMON_CFG_DRIVER_FEATURE: usize = 0x0C;
/// Offset to device_status in common config
pub const COMMON_CFG_DEVICE_STATUS: usize = 0x14;
/// Offset to queue_select in common config
pub const COMMON_CFG_QUEUE_SELECT: usize = 0x16;
/// Offset to queue_size in common config
pub const COMMON_CFG_QUEUE_SIZE: usize = 0x18;
/// Offset to queue_enable in common config
pub const COMMON_CFG_QUEUE_ENABLE: usize = 0x1C;
/// Offset to queue_notify_off in common config
pub const COMMON_CFG_QUEUE_NOTIFY_OFF: usize = 0x1E;
/// Offset to queue_desc (low) in common config
pub const COMMON_CFG_QUEUE_DESC: usize = 0x20;
/// Offset to queue_avail (low) in common config
pub const COMMON_CFG_QUEUE_AVAIL: usize = 0x28;
/// Offset to queue_used (low) in common config
pub const COMMON_CFG_QUEUE_USED: usize = 0x30;

// =============================================================================
// VirtIO MMIO Capabilities
// =============================================================================

/// Parsed VirtIO PCI capabilities - MMIO regions for device interaction
#[derive(Clone, Copy, Default)]
pub struct VirtioMmioCaps {
    /// Common configuration region
    pub common_cfg: MmioRegion,
    /// Notification region
    pub notify_cfg: MmioRegion,
    /// Notify offset multiplier (from PCI cap)
    pub notify_off_multiplier: u32,
    /// ISR status region
    pub isr_cfg: MmioRegion,
    /// Device-specific configuration region
    pub device_cfg: MmioRegion,
    /// Length of device config region
    pub device_cfg_len: u32,
}

impl VirtioMmioCaps {
    /// Create empty capabilities (no regions mapped)
    pub const fn empty() -> Self {
        Self {
            common_cfg: MmioRegion::empty(),
            notify_cfg: MmioRegion::empty(),
            notify_off_multiplier: 0,
            isr_cfg: MmioRegion::empty(),
            device_cfg: MmioRegion::empty(),
            device_cfg_len: 0,
        }
    }

    /// Check if common config is available
    #[inline]
    pub fn has_common_cfg(&self) -> bool {
        self.common_cfg.is_mapped()
    }

    /// Check if notify config is available
    #[inline]
    pub fn has_notify_cfg(&self) -> bool {
        self.notify_cfg.is_mapped()
    }

    /// Check if device config is available
    #[inline]
    pub fn has_device_cfg(&self) -> bool {
        self.device_cfg.is_mapped()
    }
}

// =============================================================================
// Device Status Helpers
// =============================================================================

/// Set the device status register
#[inline]
pub fn set_device_status(cfg: &MmioRegion, status: u8) {
    cfg.write_u8(COMMON_CFG_DEVICE_STATUS, status);
}

/// Get the device status register
#[inline]
pub fn get_device_status(cfg: &MmioRegion) -> u8 {
    cfg.read_u8(COMMON_CFG_DEVICE_STATUS)
}

/// Reset the device (set status to 0)
#[inline]
pub fn reset_device(cfg: &MmioRegion) {
    set_device_status(cfg, 0);
}
