//! PCI bus hardware definitions, configuration space constants, and device structures.
//!
//! Single source of truth for PCI constants; add new ones only when a consumer exists.

/// Vendor ID register offset (16-bit).
pub const PCI_VENDOR_ID_OFFSET: u16 = 0x00;

/// Device ID register offset (16-bit).
pub const PCI_DEVICE_ID_OFFSET: u16 = 0x02;

/// Command register offset (16-bit).
pub const PCI_COMMAND_OFFSET: u16 = 0x04;

/// Status register offset (16-bit).
pub const PCI_STATUS_OFFSET: u16 = 0x06;

/// Revision ID register offset (8-bit).
pub const PCI_REVISION_ID_OFFSET: u16 = 0x08;

/// Programming Interface offset (8-bit).
pub const PCI_PROG_IF_OFFSET: u16 = 0x09;

/// Subclass register offset (8-bit).
pub const PCI_SUBCLASS_OFFSET: u16 = 0x0A;

/// Class Code register offset (8-bit).
pub const PCI_CLASS_CODE_OFFSET: u16 = 0x0B;

/// Header Type register offset (8-bit).
pub const PCI_HEADER_TYPE_OFFSET: u16 = 0x0E;

/// Base Address Register 0 offset.
pub const PCI_BAR0_OFFSET: u16 = 0x10;

/// Capabilities pointer offset (8-bit, header type 0).
pub const PCI_CAP_PTR_OFFSET: u16 = 0x34;

/// Interrupt Line register offset (8-bit).
pub const PCI_INTERRUPT_LINE_OFFSET: u16 = 0x3C;

/// Interrupt Pin register offset (8-bit).
pub const PCI_INTERRUPT_PIN_OFFSET: u16 = 0x3D;

/// Status: Capabilities list present (bit 4).
pub const PCI_STATUS_CAP_LIST: u16 = 0x10;

/// Enable memory space access (bit 1).
pub const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;

/// Enable bus master capability (bit 2).
pub const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

/// Disable legacy INTx assertion (bit 10).
/// Must be set when MSI or MSI-X is enabled.
pub const PCI_COMMAND_INTX_DISABLE: u16 = 0x0400;

pub const PCI_CLASS_DISPLAY: u8 = 0x03;

/// Message Signaled Interrupts.
pub const PCI_CAP_ID_MSI: u8 = 0x05;

/// Vendor-specific.
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

/// PCI Express.
pub const PCI_CAP_ID_PCIE: u8 = 0x10;

/// Extended Message Signaled Interrupts.
pub const PCI_CAP_ID_MSIX: u8 = 0x11;

/// Start of the PCIe extended capability list: offsets 0x100–0xFFF of the
/// 4096-byte config space, reachable only via ECAM MMIO (not legacy port I/O).
pub const PCI_EXT_CAP_START: u16 = 0x100;

/// Advanced Error Reporting.
pub const PCI_EXT_CAP_ID_AER: u16 = 0x0001;

/// Virtual Channel.
pub const PCI_EXT_CAP_ID_VC: u16 = 0x0002;

/// Device Serial Number.
pub const PCI_EXT_CAP_ID_DSN: u16 = 0x0003;

/// Power Budgeting.
pub const PCI_EXT_CAP_ID_PWR_BUDGET: u16 = 0x0004;

/// Vendor-Specific Extended Capability.
pub const PCI_EXT_CAP_ID_VNDR: u16 = 0x000B;

/// Access Control Services.
pub const PCI_EXT_CAP_ID_ACS: u16 = 0x000D;

/// Alternative Routing-ID Interpretation.
pub const PCI_EXT_CAP_ID_ARI: u16 = 0x000E;

/// Address Translation Services.
pub const PCI_EXT_CAP_ID_ATS: u16 = 0x000F;

/// Single Root I/O Virtualization.
pub const PCI_EXT_CAP_ID_SRIOV: u16 = 0x0010;

/// Latency Tolerance Reporting.
pub const PCI_EXT_CAP_ID_LTR: u16 = 0x0018;

/// Secondary PCI Express.
pub const PCI_EXT_CAP_ID_SEC_PCIE: u16 = 0x0019;

/// L1 PM Substates.
pub const PCI_EXT_CAP_ID_L1SS: u16 = 0x001E;

/// Designated Vendor-Specific.
pub const PCI_EXT_CAP_ID_DVSEC: u16 = 0x0023;

/// Data Link Feature.
pub const PCI_EXT_CAP_ID_DLF: u16 = 0x0025;

/// Physical Layer 16.0 GT/s.
pub const PCI_EXT_CAP_ID_PL16G: u16 = 0x0026;

/// VirtIO vendor ID (Red Hat).
pub const PCI_VENDOR_ID_VIRTIO: u16 = 0x1AF4;

/// Invalid vendor ID (no device present).
pub const PCI_VENDOR_ID_INVALID: u16 = 0xFFFF;

pub const PCI_MAX_BUSES: usize = 256;

pub const PCI_MAX_DEVICES: usize = 256;

pub const PCI_MAX_BARS: usize = 6;

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

    /// Physical address of a `len`-byte window at `offset` within this BAR,
    /// or `None` if it does not fit.
    ///
    /// The offsets and lengths that reach this are read from device config
    /// space, which a hostile function controls. Unchecked, they map or index
    /// MMIO anywhere in the physical address space.
    pub fn window(&self, offset: u64, len: u64) -> Option<u64> {
        if self.base == 0 || self.is_io != 0 || len == 0 {
            return None;
        }
        let end = offset.checked_add(len)?;
        if end > self.size {
            return None;
        }
        self.base.checked_add(offset)
    }
}

/// A PCI capability found in the config-space linked list. The header carries an
/// 8-bit ID (`PCI_CAP_ID_*`) and a variable-length region starting at `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciCapability {
    /// Byte offset of this capability header in configuration space.
    pub offset: u16,
    pub id: u8,
}

/// A PCIe extended capability found in extended config space. Headers are 32-bit
/// DWORDs at offsets ≥ 0x100, ECAM-only:
///   bits [15:0]  — capability ID (see `PCI_EXT_CAP_ID_*`)
///   bits [19:16] — capability version
///   bits [31:20] — next capability offset (0 = end of list)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciExtCapability {
    /// Byte offset of this extended capability header in configuration space.
    pub offset: u16,
    pub id: u16,
    pub version: u8,
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
    pub msi_cap_offset: Option<u16>,
    pub msix_cap_offset: Option<u16>,
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
            msi_cap_offset: None,
            msix_cap_offset: None,
        }
    }

    #[inline]
    pub const fn has_msi(&self) -> bool {
        self.msi_cap_offset.is_some()
    }

    #[inline]
    pub const fn has_msix(&self) -> bool {
        self.msix_cap_offset.is_some()
    }
}
