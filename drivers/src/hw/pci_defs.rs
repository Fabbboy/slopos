//! PCI bus hardware definitions and configuration space constants.

// ============================================================================
// Configuration Space I/O Ports
// ============================================================================

/// PCI configuration address port (type 1 mechanism)
pub(crate) const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
/// PCI configuration data port
pub(crate) const PCI_CONFIG_DATA: u16 = 0xCFC;

// ============================================================================
// Configuration Space Register Offsets
// ============================================================================

/// Vendor ID register offset (16-bit)
pub(crate) const PCI_VENDOR_ID_OFFSET: u8 = 0x00;
/// Device ID register offset (16-bit)
pub(crate) const PCI_DEVICE_ID_OFFSET: u8 = 0x02;
/// Command register offset (16-bit)
pub const PCI_COMMAND_OFFSET: u8 = 0x04;
/// Revision ID register offset
pub(crate) const PCI_REVISION_ID_OFFSET: u8 = 0x08;
/// Programming Interface offset
pub(crate) const PCI_PROG_IF_OFFSET: u8 = 0x09;
/// Subclass register offset
pub(crate) const PCI_SUBCLASS_OFFSET: u8 = 0x0A;
/// Class Code register offset
pub(crate) const PCI_CLASS_CODE_OFFSET: u8 = 0x0B;
/// Header Type register offset
pub(crate) const PCI_HEADER_TYPE_OFFSET: u8 = 0x0E;
/// Base Address Register 0 offset
pub(crate) const PCI_BAR0_OFFSET: u8 = 0x10;
/// Interrupt Line register offset
pub(crate) const PCI_INTERRUPT_LINE_OFFSET: u8 = 0x3C;
/// Interrupt Pin register offset
pub(crate) const PCI_INTERRUPT_PIN_OFFSET: u8 = 0x3D;

// ============================================================================
// Header Type Flags
// ============================================================================

/// Mask to extract header type (bits 0-6)
pub(crate) const PCI_HEADER_TYPE_MASK: u8 = 0x7F;
/// Multi-function device flag (bit 7)
pub(crate) const PCI_HEADER_TYPE_MULTI_FUNCTION: u8 = 0x80;
/// Standard device header type
pub(crate) const PCI_HEADER_TYPE_DEVICE: u8 = 0x00;
/// PCI-to-PCI bridge header type
pub(crate) const PCI_HEADER_TYPE_BRIDGE: u8 = 0x01;

// ============================================================================
// BAR (Base Address Register) Flags
// ============================================================================

/// I/O space indicator (bit 0)
pub(crate) const PCI_BAR_IO_SPACE: u32 = 0x1;
/// I/O address mask (bits 2-31)
pub(crate) const PCI_BAR_IO_ADDRESS_MASK: u32 = 0xFFFFFFFC;
/// Memory type mask (bits 1-2)
pub(crate) const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;
/// 64-bit memory type
pub(crate) const PCI_BAR_MEM_TYPE_64: u32 = 0x4;
/// Prefetchable flag (bit 3)
pub(crate) const PCI_BAR_MEM_PREFETCHABLE: u32 = 0x8;
/// Memory address mask (bits 4-31)
pub(crate) const PCI_BAR_MEM_ADDRESS_MASK: u32 = 0xFFFFFFF0;

/// Maximum number of BARs per device
pub const PCI_MAX_BARS: usize = 6;

// ============================================================================
// Command Register Bits
// ============================================================================

/// Enable memory space access
pub(crate) const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;
/// Enable bus master capability
pub(crate) const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

// ============================================================================
// Device Classes
// ============================================================================

/// Display controller class
pub(crate) const PCI_CLASS_DISPLAY: u8 = 0x03;

// ============================================================================
// Known Vendor/Device IDs
// ============================================================================

/// VirtIO vendor ID
pub(crate) const PCI_VENDOR_ID_VIRTIO: u16 = 0x1AF4;
/// VirtIO GPU device ID (modern)
pub(crate) const PCI_DEVICE_ID_VIRTIO_GPU: u16 = 0x1050;
/// VirtIO GPU device ID (transitional)
pub(crate) const PCI_DEVICE_ID_VIRTIO_GPU_TRANS: u16 = 0x1010;

// ============================================================================
// Limits
// ============================================================================

/// Maximum number of PCI buses
pub(crate) const PCI_MAX_BUSES: usize = 256;
/// Maximum tracked PCI devices
pub(crate) const PCI_MAX_DEVICES: usize = 256;
/// Maximum registered PCI drivers
pub(crate) const PCI_DRIVER_MAX: usize = 16;
