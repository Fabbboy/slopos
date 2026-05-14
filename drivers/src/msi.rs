//! MSI (Message Signaled Interrupts) support for PCI devices.
//!
//! MSI allows PCI devices to deliver interrupts by writing a message directly
//! to the LAPIC — no IOAPIC redirection involved.  This eliminates shared IRQ
//! lines, reduces latency, and is mandatory for PCIe devices per spec.
//!
//! ## Usage
//!
//! ```ignore
//! use slopos_core::irq::{msi_alloc_vector, msi_register_handler};
//! use slopos_drivers::msi;
//!
//! let cap = msi::msi_read_capability(bus, dev, func, cap_offset);
//! let vector = msi_alloc_vector().expect("MSI vector exhausted");
//! msi_register_handler(vector, my_handler, ctx, bdf);
//! msi::msi_configure(bus, dev, func, &cap, vector, apic_id).unwrap();
//! ```
//!
//! ## Register layout reference (PCI Local Bus Spec §6.8)
//!
//! ```text
//! Offset  Size  Field
//! +0x00   8     Cap ID (0x05) | Next Pointer
//! +0x02   16    Message Control
//! +0x04   32    Message Address (lower)
//! +0x08   32    Message Address (upper) — only if 64-bit capable
//! +0x08/C 16    Message Data
//! +0x10/14 32   Mask Bits — only if per-vector masking capable
//! +0x14/18 32   Pending Bits — only if per-vector masking capable
//! ```

use crate::msi_common;
use crate::pci::{pci_config_read16, pci_config_read32, pci_config_write16, pci_config_write32};
use slopos_ostd::klog_info;

// =============================================================================
// MSI Message Control register bits (offset +2 from capability base)
// =============================================================================

const MSI_CTRL_ENABLE: u16 = 1 << 0;
const MSI_CTRL_MMC_SHIFT: u16 = 1;
const MSI_CTRL_MME_MASK: u16 = 0x7 << 4;
const MSI_CTRL_64BIT: u16 = 1 << 7;
const MSI_CTRL_PVM: u16 = 1 << 8;

// =============================================================================
// Register offsets (relative to capability base)
// =============================================================================

const MSI_REG_CONTROL: u16 = 0x02;
const MSI_REG_ADDR_LO: u16 = 0x04;
const MSI_REG_ADDR_HI: u16 = 0x08;
const MSI_REG_DATA_32: u16 = 0x08;
const MSI_REG_DATA_64: u16 = 0x0C;
const MSI_REG_MASK_32: u16 = 0x10;
const MSI_REG_MASK_64: u16 = 0x14;

// =============================================================================
// Public types
// =============================================================================

/// Parsed MSI capability information for a PCI device.
#[derive(Debug, Clone, Copy)]
pub struct MsiCapability {
    /// Byte offset of the MSI capability in PCI config space.
    pub cap_offset: u16,
    /// Raw Message Control register value at parse time.
    pub control: u16,
    /// Whether the device supports 64-bit message addresses.
    pub is_64bit: bool,
    /// Whether the device supports per-vector masking.
    pub has_per_vector_masking: bool,
    /// log₂ of the maximum vectors the device can generate (0–5 → 1–32).
    pub multi_message_capable: u8,
}

impl MsiCapability {
    /// Maximum number of vectors the device can generate (1, 2, 4, 8, 16, or 32).
    #[inline]
    pub const fn max_vectors(&self) -> u8 {
        1u8 << self.multi_message_capable
    }

    /// Config-space offset of the Message Data register.
    #[inline]
    const fn data_offset(&self) -> u16 {
        if self.is_64bit {
            self.cap_offset + MSI_REG_DATA_64
        } else {
            self.cap_offset + MSI_REG_DATA_32
        }
    }

    /// Config-space offset of the Mask Bits register, if supported.
    #[inline]
    pub const fn mask_offset(&self) -> Option<u16> {
        if !self.has_per_vector_masking {
            return None;
        }
        Some(if self.is_64bit {
            self.cap_offset + MSI_REG_MASK_64
        } else {
            self.cap_offset + MSI_REG_MASK_32
        })
    }

    /// Whether MSI is currently enabled on this device.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        (self.control & MSI_CTRL_ENABLE) != 0
    }
}

/// Errors that can occur during MSI configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiError {
    /// The supplied vector number is below 32 (reserved for CPU exceptions).
    InvalidVector,
    /// The device does not have an MSI capability.
    NoCapability,
}

// =============================================================================
// Capability parsing
// =============================================================================

/// Read and parse the MSI capability structure for a PCI device.
///
/// `cap_offset` is the config-space byte offset of the MSI capability header
/// (obtained from [`PciDeviceInfo::msi_cap_offset`] or
/// [`pci_find_capability`]).
pub fn msi_read_capability(bus: u8, dev: u8, func: u8, cap_offset: u16) -> MsiCapability {
    let control = pci_config_read16(bus, dev, func, cap_offset + MSI_REG_CONTROL);
    MsiCapability {
        cap_offset,
        control,
        is_64bit: (control & MSI_CTRL_64BIT) != 0,
        has_per_vector_masking: (control & MSI_CTRL_PVM) != 0,
        multi_message_capable: ((control >> MSI_CTRL_MMC_SHIFT) & 0x7) as u8,
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configure MSI for a PCI device to deliver a single interrupt.
///
/// Programs the MSI capability registers so that the device writes an MSI
/// message targeting `target_apic_id` with interrupt `vector`.  Legacy INTx
/// is disabled and MSI is enabled atomically at the end.
///
/// # Programming sequence
///
/// 1. Disable MSI (clear enable bit).
/// 2. Write Message Address (LAPIC base + destination APIC ID).
/// 3. Write Message Upper Address (always 0 on x86).
/// 4. Write Message Data (vector + fixed delivery + edge trigger).
/// 5. Set multi-message enable = 0 (single vector).
/// 6. Disable legacy INTx in Command register.
/// 7. Enable MSI.
///
/// # Errors
///
/// Returns [`MsiError::InvalidVector`] if `vector < 32`.
pub fn msi_configure(
    bus: u8,
    dev: u8,
    func: u8,
    cap: &MsiCapability,
    vector: u8,
    target_apic_id: u8,
) -> Result<(), MsiError> {
    if !msi_common::is_valid_vector(vector) {
        return Err(MsiError::InvalidVector);
    }

    let cap_off = cap.cap_offset;

    // 1. Disable MSI while we reprogram the registers.
    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSI_REG_CONTROL);
    ctrl &= !MSI_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    // 2. Message Address — physical mode, target APIC ID.
    pci_config_write32(
        bus,
        dev,
        func,
        cap_off + MSI_REG_ADDR_LO,
        msi_common::lapic_msg_addr(target_apic_id),
    );

    // 3. Message Upper Address — always 0 on x86.
    if cap.is_64bit {
        pci_config_write32(bus, dev, func, cap_off + MSI_REG_ADDR_HI, 0);
    }

    // 4. Message Data — vector, fixed delivery mode, edge triggered.
    pci_config_write16(
        bus,
        dev,
        func,
        cap.data_offset(),
        msi_common::lapic_msg_data(vector) as u16,
    );

    // 5. Request exactly 1 vector (multi-message enable = 0).
    ctrl = pci_config_read16(bus, dev, func, cap_off + MSI_REG_CONTROL);
    ctrl &= !MSI_CTRL_ENABLE;
    ctrl &= !MSI_CTRL_MME_MASK;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    // 6. Disable legacy INTx.
    msi_common::disable_intx(bus, dev, func);

    // 7. Enable MSI.
    ctrl |= MSI_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    klog_info!(
        "MSI: Configured BDF {}:{}.{} -> vector 0x{:02x}, APIC ID {}{}{}",
        bus,
        dev,
        func,
        vector,
        target_apic_id,
        if cap.is_64bit { ", 64-bit" } else { "" },
        if cap.has_per_vector_masking {
            ", PVM"
        } else {
            ""
        },
    );

    Ok(())
}

/// Disable MSI for a device and re-enable legacy INTx.
pub fn msi_disable(bus: u8, dev: u8, func: u8, cap: &MsiCapability) {
    let cap_off = cap.cap_offset;

    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSI_REG_CONTROL);
    ctrl &= !MSI_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    msi_common::restore_intx(bus, dev, func);

    klog_info!("MSI: Disabled for BDF {}:{}.{}", bus, dev, func);
}

/// Mask a specific MSI vector (only if per-vector masking is supported).
///
/// `vector_idx` is 0-based within the device's allocated vectors.
pub fn msi_mask_vector(bus: u8, dev: u8, func: u8, cap: &MsiCapability, vector_idx: u8) {
    if let Some(mask_off) = cap.mask_offset() {
        let mask = pci_config_read32(bus, dev, func, mask_off);
        pci_config_write32(bus, dev, func, mask_off, mask | (1u32 << vector_idx));
    }
}

/// Unmask a specific MSI vector (only if per-vector masking is supported).
///
/// `vector_idx` is 0-based within the device's allocated vectors.
pub fn msi_unmask_vector(bus: u8, dev: u8, func: u8, cap: &MsiCapability, vector_idx: u8) {
    if let Some(mask_off) = cap.mask_offset() {
        let mask = pci_config_read32(bus, dev, func, mask_off);
        pci_config_write32(bus, dev, func, mask_off, mask & !(1u32 << vector_idx));
    }
}

/// Re-read the Message Control register to refresh capability state.
pub fn msi_refresh_control(bus: u8, dev: u8, func: u8, cap: &mut MsiCapability) {
    cap.control = pci_config_read16(bus, dev, func, cap.cap_offset + MSI_REG_CONTROL);
}
