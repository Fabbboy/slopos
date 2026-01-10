//! PCI bus hardware definitions, configuration space constants, and device structures.
//!
//! This module provides:
//! - Constants for PCI configuration space access (register offsets, header types, BAR flags)
//! - Known vendor/device IDs
//! - Data structures for PCI device enumeration (`PciBarInfo`, `PciDeviceInfo`, `PciGpuInfo`)
//!
//! All PCI structures are `#[repr(C)]` for ABI stability between kernel subsystems.

// =============================================================================
// Configuration Space Register Offsets
// =============================================================================

/// Vendor ID register offset (16-bit).
pub const PCI_VENDOR_ID_OFFSET: u8 = 0x00;

/// Device ID register offset (16-bit).
pub const PCI_DEVICE_ID_OFFSET: u8 = 0x02;

/// Command register offset (16-bit).
pub const PCI_COMMAND_OFFSET: u8 = 0x04;

/// Status register offset (16-bit).
pub const PCI_STATUS_OFFSET: u8 = 0x06;

/// Revision ID register offset (8-bit).
pub const PCI_REVISION_ID_OFFSET: u8 = 0x08;

/// Programming Interface offset (8-bit).
pub const PCI_PROG_IF_OFFSET: u8 = 0x09;

/// Subclass register offset (8-bit).
pub const PCI_SUBCLASS_OFFSET: u8 = 0x0A;

/// Class Code register offset (8-bit).
pub const PCI_CLASS_CODE_OFFSET: u8 = 0x0B;

/// Header Type register offset (8-bit).
pub const PCI_HEADER_TYPE_OFFSET: u8 = 0x0E;

/// Base Address Register 0 offset.
pub const PCI_BAR0_OFFSET: u8 = 0x10;

/// Interrupt Line register offset (8-bit).
pub const PCI_INTERRUPT_LINE_OFFSET: u8 = 0x3C;

/// Interrupt Pin register offset (8-bit).
pub const PCI_INTERRUPT_PIN_OFFSET: u8 = 0x3D;

// =============================================================================
// Header Type Flags
// =============================================================================

/// Mask to extract header type (bits 0-6).
pub const PCI_HEADER_TYPE_MASK: u8 = 0x7F;

/// Multi-function device flag (bit 7).
pub const PCI_HEADER_TYPE_MULTI_FUNCTION: u8 = 0x80;

/// Standard device header type (type 0).
pub const PCI_HEADER_TYPE_DEVICE: u8 = 0x00;

/// PCI-to-PCI bridge header type (type 1).
pub const PCI_HEADER_TYPE_BRIDGE: u8 = 0x01;

// =============================================================================
// BAR (Base Address Register) Flags
// =============================================================================

/// I/O space indicator (bit 0 = 1).
pub const PCI_BAR_IO_SPACE: u32 = 0x1;

/// I/O address mask (bits 2-31).
pub const PCI_BAR_IO_ADDRESS_MASK: u32 = 0xFFFF_FFFC;

/// Memory type mask (bits 1-2).
pub const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;

/// 64-bit memory type (bits 1-2 = 10).
pub const PCI_BAR_MEM_TYPE_64: u32 = 0x4;

/// Prefetchable flag (bit 3).
pub const PCI_BAR_MEM_PREFETCHABLE: u32 = 0x8;

/// Memory address mask (bits 4-31).
pub const PCI_BAR_MEM_ADDRESS_MASK: u32 = 0xFFFF_FFF0;

/// Maximum number of BARs per device.
pub const PCI_MAX_BARS: usize = 6;

// =============================================================================
// Command Register Bits
// =============================================================================

/// Enable I/O space access (bit 0).
pub const PCI_COMMAND_IO_SPACE: u16 = 0x0001;

/// Enable memory space access (bit 1).
pub const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;

/// Enable bus master capability (bit 2).
pub const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

/// Enable special cycles (bit 3).
pub const PCI_COMMAND_SPECIAL_CYCLES: u16 = 0x0008;

/// Disable interrupts (bit 10).
pub const PCI_COMMAND_INTERRUPT_DISABLE: u16 = 0x0400;

// =============================================================================
// Device Classes
// =============================================================================

/// Unclassified device.
pub const PCI_CLASS_UNCLASSIFIED: u8 = 0x00;

/// Mass storage controller.
pub const PCI_CLASS_MASS_STORAGE: u8 = 0x01;

/// Network controller.
pub const PCI_CLASS_NETWORK: u8 = 0x02;

/// Display controller.
pub const PCI_CLASS_DISPLAY: u8 = 0x03;

/// Multimedia controller.
pub const PCI_CLASS_MULTIMEDIA: u8 = 0x04;

/// Memory controller.
pub const PCI_CLASS_MEMORY: u8 = 0x05;

/// Bridge device.
pub const PCI_CLASS_BRIDGE: u8 = 0x06;

/// Simple communication controller.
pub const PCI_CLASS_SIMPLE_COMM: u8 = 0x07;

/// Base system peripheral.
pub const PCI_CLASS_BASE_PERIPHERAL: u8 = 0x08;

/// Input device controller.
pub const PCI_CLASS_INPUT: u8 = 0x09;

/// Serial bus controller.
pub const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;

// =============================================================================
// Known Vendor/Device IDs
// =============================================================================

/// VirtIO vendor ID (Red Hat).
pub const PCI_VENDOR_ID_VIRTIO: u16 = 0x1AF4;

/// VirtIO GPU device ID (modern).
pub const PCI_DEVICE_ID_VIRTIO_GPU: u16 = 0x1050;

/// VirtIO GPU device ID (transitional).
pub const PCI_DEVICE_ID_VIRTIO_GPU_TRANS: u16 = 0x1010;

/// Invalid vendor ID (no device present).
pub const PCI_VENDOR_ID_INVALID: u16 = 0xFFFF;

// =============================================================================
// Limits
// =============================================================================

/// Maximum number of PCI buses.
pub const PCI_MAX_BUSES: usize = 256;

/// Maximum number of devices per bus.
pub const PCI_MAX_DEVICES_PER_BUS: usize = 32;

/// Maximum number of functions per device.
pub const PCI_MAX_FUNCTIONS: usize = 8;

/// Maximum tracked PCI devices.
pub const PCI_MAX_DEVICES: usize = 256;

/// Maximum registered PCI drivers.
pub const PCI_DRIVER_MAX: usize = 16;

// =============================================================================
// PCI Device Structures
// =============================================================================

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PciBarInfo {
    pub base: u64,
    pub size: u64,
    pub is_io: u8,
    pub is_64bit: u8,
    pub prefetchable: u8,
}

impl PciBarInfo {
    pub const fn zeroed() -> Self {
        Self {
            base: 0,
            size: 0,
            is_io: 0,
            is_64bit: 0,
            prefetchable: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub bar_count: u8,
    pub bars: [PciBarInfo; PCI_MAX_BARS],
}

impl PciDeviceInfo {
    pub const fn zeroed() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            header_type: 0,
            irq_line: 0,
            irq_pin: 0,
            bar_count: 0,
            bars: [PciBarInfo::zeroed(); PCI_MAX_BARS],
        }
    }
}
