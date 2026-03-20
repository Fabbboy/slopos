//! MSI-X (Extended Message Signaled Interrupts) support for PCI devices.
//!
//! MSI-X extends MSI with a per-entry table stored in BAR memory, supporting
//! up to 2048 vectors per device with individual per-vector masking.  The table
//! is accessed via MMIO rather than configuration space registers, making MSI-X
//! the preferred interrupt mechanism for high-performance devices like VirtIO
//! multi-queue networking.
//!
//! ## Usage
//!
//! ```ignore
//! use slopos_core::irq::{msi_alloc_vector, msi_register_handler};
//! use slopos_drivers::msix;
//!
//! let cap = msix::msix_read_capability(bus, dev, func, cap_offset);
//! let table = msix::msix_map_table(&device_info, &cap).expect("MSI-X map failed");
//! let vector = msi_alloc_vector().expect("MSI vector exhausted");
//! msi_register_handler(vector, my_handler, ctx, bdf);
//! msix::msix_configure(&table, 0, vector, apic_id).unwrap();
//! msix::msix_enable(bus, dev, func, &cap);
//! ```
//!
//! ## Register layout reference (PCI Local Bus Spec §6.8.2)
//!
//! ```text
//! Config space (capability header):
//! Offset  Size  Field
//! +0x00   8     Cap ID (0x11) | Next Pointer
//! +0x02   16    Message Control (table size, function mask, enable)
//! +0x04   32    Table Offset / BIR
//! +0x08   32    PBA Offset / BIR
//!
//! BAR memory (MSI-X table, 16 bytes per entry):
//! +0x00   32    Message Address (lower)
//! +0x04   32    Message Address (upper)
//! +0x08   32    Message Data
//! +0x0C   32    Vector Control (bit 0 = mask)
//! ```

use crate::msi_common;
use crate::pci::{PciDeviceInfo, pci_config_read16, pci_config_read32, pci_config_write16};
use crate::pci_defs::PCI_MAX_BARS;
use slopos_abi::addr::PhysAddr;
use slopos_lib::klog_info;
use slopos_mm::mmio::MmioRegion;

// =============================================================================
// MSI-X Message Control register bits (offset +2 from capability base)
// =============================================================================

/// MSI-X enable bit (bit 15 of Message Control).
const MSIX_CTRL_ENABLE: u16 = 1 << 15;

/// Function mask bit (bit 14 of Message Control).
/// When set, all table entries are masked regardless of per-vector mask bits.
const MSIX_CTRL_FUNCTION_MASK: u16 = 1 << 14;

/// Table size mask (bits 10:0 of Message Control).
/// Encoded as N-1: 0 means 1 entry, 2047 means 2048 entries.
const MSIX_CTRL_TABLE_SIZE_MASK: u16 = 0x7FF;

// =============================================================================
// Register offsets (relative to capability base)
// =============================================================================

const MSIX_REG_CONTROL: u16 = 0x02;
const MSIX_REG_TABLE_OFFSET: u16 = 0x04;
const MSIX_REG_PBA_OFFSET: u16 = 0x08;

// =============================================================================
// Table/PBA BIR and offset extraction
// =============================================================================

/// BAR Indicator Register mask (bits 2:0 of Table/PBA Offset register).
const MSIX_BIR_MASK: u32 = 0x7;

/// Offset mask (bits 31:3 of Table/PBA Offset register), DWORD-aligned.
const MSIX_OFFSET_MASK: u32 = !0x7;

// =============================================================================
// MSI-X table entry layout (16 bytes per entry, MMIO)
// =============================================================================

/// Byte offset of Message Address (lower 32 bits) within a table entry.
const MSIX_ENTRY_ADDR_LO: usize = 0x00;

/// Byte offset of Message Address (upper 32 bits) within a table entry.
const MSIX_ENTRY_ADDR_HI: usize = 0x04;

/// Byte offset of Message Data within a table entry.
const MSIX_ENTRY_DATA: usize = 0x08;

/// Byte offset of Vector Control within a table entry.
const MSIX_ENTRY_VECTOR_CTRL: usize = 0x0C;

/// Size of a single MSI-X table entry in bytes.
const MSIX_ENTRY_SIZE: usize = 16;

/// Vector Control: mask bit (bit 0).  When set, the entry is masked.
const MSIX_ENTRY_CTRL_MASK: u32 = 1;

// =============================================================================
// Public types
// =============================================================================

/// Parsed MSI-X capability information from PCI configuration space.
///
/// This captures the static capability metadata.  The actual MSI-X table
/// lives in BAR memory and must be mapped separately via [`msix_map_table`].
#[derive(Debug, Clone, Copy)]
pub struct MsixCapability {
    /// Byte offset of the MSI-X capability in PCI config space.
    pub cap_offset: u16,
    /// Raw Message Control register value at parse time.
    pub control: u16,
    /// Number of table entries (1–2048).
    pub table_size: u16,
    /// BAR index containing the MSI-X table (0–5).
    pub table_bar: u8,
    /// Byte offset of the table within the BAR.
    pub table_offset: u32,
    /// BAR index containing the Pending Bit Array (0–5).
    pub pba_bar: u8,
    /// Byte offset of the PBA within the BAR.
    pub pba_offset: u32,
}

impl MsixCapability {
    /// Whether MSI-X is currently enabled on this device.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        (self.control & MSIX_CTRL_ENABLE) != 0
    }

    /// Whether the function-level mask is active.
    #[inline]
    pub const fn is_function_masked(&self) -> bool {
        (self.control & MSIX_CTRL_FUNCTION_MASK) != 0
    }
}

/// Mapped MSI-X table and Pending Bit Array.
///
/// Created by [`msix_map_table`].  Holds the MMIO regions for the MSI-X
/// table and PBA, and the table size for bounds checking.
///
/// The table is read/written via MMIO — not through PCI configuration space.
/// Each entry is 16 bytes: `{ addr_lo, addr_hi, data, vector_control }`.
#[derive(Debug, Clone, Copy)]
pub struct MsixTable {
    /// Mapped MSI-X table region.
    table: MmioRegion,
    /// Mapped Pending Bit Array region.
    pba: MmioRegion,
    /// Number of entries in the table (1–2048).
    table_size: u16,
}

impl MsixTable {
    /// Number of entries in the MSI-X table.
    #[inline]
    pub const fn table_size(&self) -> u16 {
        self.table_size
    }

    /// Whether the table is successfully mapped and usable.
    #[inline]
    pub fn is_mapped(&self) -> bool {
        self.table.is_mapped()
    }

    /// Read the Vector Control field for a table entry.
    ///
    /// Returns `None` if `entry_idx` is out of range.
    pub fn read_vector_control(&self, entry_idx: u16) -> Option<u32> {
        if entry_idx >= self.table_size {
            return None;
        }
        let offset = (entry_idx as usize) * MSIX_ENTRY_SIZE + MSIX_ENTRY_VECTOR_CTRL;
        Some(self.table.read::<u32>(offset))
    }

    /// Check whether a specific table entry's interrupt is pending (PBA bit set).
    ///
    /// Returns `None` if `entry_idx` is out of range or PBA is not mapped.
    pub fn is_pending(&self, entry_idx: u16) -> Option<bool> {
        if entry_idx >= self.table_size || !self.pba.is_mapped() {
            return None;
        }
        let qword_idx = (entry_idx / 64) as usize;
        let bit = entry_idx % 64;
        let pba_word: u64 = self.pba.read::<u64>(qword_idx * 8);
        Some((pba_word & (1u64 << bit)) != 0)
    }

    /// Read the Message Data field for a table entry.
    ///
    /// Bits 7:0 contain the interrupt vector.  Returns `None` if
    /// `entry_idx` is out of range.
    pub fn read_msg_data(&self, entry_idx: u16) -> Option<u32> {
        if entry_idx >= self.table_size {
            return None;
        }
        let offset = (entry_idx as usize) * MSIX_ENTRY_SIZE + MSIX_ENTRY_DATA;
        Some(self.table.read::<u32>(offset))
    }

    /// Read the Message Address (low 32 bits) for a table entry.
    ///
    /// Contains the destination APIC ID and addressing mode.  Returns
    /// `None` if `entry_idx` is out of range.
    pub fn read_msg_addr_lo(&self, entry_idx: u16) -> Option<u32> {
        if entry_idx >= self.table_size {
            return None;
        }
        let offset = (entry_idx as usize) * MSIX_ENTRY_SIZE + MSIX_ENTRY_ADDR_LO;
        Some(self.table.read::<u32>(offset))
    }
}

/// Errors that can occur during MSI-X operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsixError {
    /// The supplied vector number is below 32 (reserved for CPU exceptions).
    InvalidVector,
    /// The table entry index exceeds the device's table size.
    InvalidEntry,
    /// The BAR required for the MSI-X table is not present or is I/O space.
    BarNotAvailable,
    /// MMIO mapping of the table or PBA region failed.
    MappingFailed,
    /// The MSI-X table has not been mapped yet.
    TableNotMapped,
}

// =============================================================================
// Capability parsing
// =============================================================================

/// Read and parse the MSI-X capability structure from PCI configuration space.
///
/// `cap_offset` is the config-space byte offset of the MSI-X capability header
/// (obtained from [`PciDeviceInfo::msix_cap_offset`] or
/// [`pci_find_capability`]).
pub fn msix_read_capability(bus: u8, dev: u8, func: u8, cap_offset: u16) -> MsixCapability {
    let control = pci_config_read16(bus, dev, func, cap_offset + MSIX_REG_CONTROL);
    let table_size = (control & MSIX_CTRL_TABLE_SIZE_MASK) + 1;

    let table_dword = pci_config_read32(bus, dev, func, cap_offset + MSIX_REG_TABLE_OFFSET);
    let table_bar = (table_dword & MSIX_BIR_MASK) as u8;
    let table_offset = table_dword & MSIX_OFFSET_MASK;

    let pba_dword = pci_config_read32(bus, dev, func, cap_offset + MSIX_REG_PBA_OFFSET);
    let pba_bar = (pba_dword & MSIX_BIR_MASK) as u8;
    let pba_offset = pba_dword & MSIX_OFFSET_MASK;

    MsixCapability {
        cap_offset,
        control,
        table_size,
        table_bar,
        table_offset,
        pba_bar,
        pba_offset,
    }
}

// =============================================================================
// Table mapping
// =============================================================================

/// Map the MSI-X table and Pending Bit Array into kernel virtual memory.
///
/// Reads the BAR addresses from `device` and maps the MMIO regions for both
/// the MSI-X table and PBA.  The returned [`MsixTable`] is used for all
/// subsequent entry configuration via [`msix_configure`].
///
/// # Errors
///
/// Returns [`MsixError::BarNotAvailable`] if the BAR is missing or is I/O space.
/// Returns [`MsixError::MappingFailed`] if `MmioRegion::map()` fails.
pub fn msix_map_table(
    device: &PciDeviceInfo,
    cap: &MsixCapability,
) -> Result<MsixTable, MsixError> {
    // --- Map the MSI-X table region ---
    let table_bar_idx = cap.table_bar as usize;
    if table_bar_idx >= PCI_MAX_BARS {
        return Err(MsixError::BarNotAvailable);
    }
    let table_bar = &device.bars[table_bar_idx];
    if table_bar.base == 0 || table_bar.is_io != 0 {
        return Err(MsixError::BarNotAvailable);
    }

    let table_bytes = (cap.table_size as usize) * MSIX_ENTRY_SIZE;
    let table_phys = PhysAddr::new(table_bar.base.wrapping_add(cap.table_offset as u64));
    let table_region = MmioRegion::map(table_phys, table_bytes).ok_or(MsixError::MappingFailed)?;

    // --- Map the PBA region ---
    let pba_bar_idx = cap.pba_bar as usize;
    if pba_bar_idx >= PCI_MAX_BARS {
        return Err(MsixError::BarNotAvailable);
    }
    let pba_bar = &device.bars[pba_bar_idx];
    if pba_bar.base == 0 || pba_bar.is_io != 0 {
        return Err(MsixError::BarNotAvailable);
    }

    // PBA: one bit per table entry, rounded up to QWORD granularity.
    let pba_bytes = (((cap.table_size as usize) + 63) / 64) * 8;
    let pba_phys = PhysAddr::new(pba_bar.base.wrapping_add(cap.pba_offset as u64));
    let pba_region = MmioRegion::map(pba_phys, pba_bytes).ok_or(MsixError::MappingFailed)?;

    klog_info!(
        "MSI-X: Mapped table for BDF {}:{}.{}: {} entries, table BAR{} offset 0x{:x}, PBA BAR{} offset 0x{:x}",
        device.bus,
        device.device,
        device.function,
        cap.table_size,
        cap.table_bar,
        cap.table_offset,
        cap.pba_bar,
        cap.pba_offset,
    );

    Ok(MsixTable {
        table: table_region,
        pba: pba_region,
        table_size: cap.table_size,
    })
}

// =============================================================================
// Configuration
// =============================================================================

/// Configure a single MSI-X table entry to deliver an interrupt.
///
/// Programs the table entry at `entry_idx` with the x86 LAPIC message address
/// and data for the specified `vector` and `target_apic_id`.  The entry is
/// masked during programming and unmasked once the address/data are written.
///
/// # Programming sequence
///
/// 1. Mask the entry (set vector control bit 0).
/// 2. Write Message Address (LAPIC base + destination APIC ID).
/// 3. Write Message Upper Address (always 0 on x86).
/// 4. Write Message Data (vector + fixed delivery + edge trigger).
/// 5. Unmask the entry (clear vector control bit 0).
///
/// # Errors
///
/// Returns [`MsixError::InvalidVector`] if `vector < 32`.
/// Returns [`MsixError::InvalidEntry`] if `entry_idx >= table_size`.
/// Returns [`MsixError::TableNotMapped`] if the table has not been mapped.
pub fn msix_configure(
    table: &MsixTable,
    entry_idx: u16,
    vector: u8,
    target_apic_id: u8,
) -> Result<(), MsixError> {
    if !msi_common::is_valid_vector(vector) {
        return Err(MsixError::InvalidVector);
    }
    if entry_idx >= table.table_size {
        return Err(MsixError::InvalidEntry);
    }
    if !table.is_mapped() {
        return Err(MsixError::TableNotMapped);
    }

    let base = (entry_idx as usize) * MSIX_ENTRY_SIZE;

    // 1. Mask the entry while programming.
    table
        .table
        .write::<u32>(base + MSIX_ENTRY_VECTOR_CTRL, MSIX_ENTRY_CTRL_MASK);

    // 2–3. Message Address (lo = LAPIC target, hi = 0 on x86).
    table.table.write::<u32>(
        base + MSIX_ENTRY_ADDR_LO,
        msi_common::lapic_msg_addr(target_apic_id),
    );
    table.table.write::<u32>(base + MSIX_ENTRY_ADDR_HI, 0);

    // 4. Message Data.
    table
        .table
        .write::<u32>(base + MSIX_ENTRY_DATA, msi_common::lapic_msg_data(vector));

    // 5. Unmask the entry.
    table.table.write::<u32>(base + MSIX_ENTRY_VECTOR_CTRL, 0);

    Ok(())
}

/// Mask a specific MSI-X table entry.
///
/// Returns `false` if the entry index is out of range or the table is not mapped.
pub fn msix_mask_entry(table: &MsixTable, entry_idx: u16) -> bool {
    if entry_idx >= table.table_size || !table.is_mapped() {
        return false;
    }
    let offset = (entry_idx as usize) * MSIX_ENTRY_SIZE + MSIX_ENTRY_VECTOR_CTRL;
    let ctrl = table.table.read::<u32>(offset);
    table
        .table
        .write::<u32>(offset, ctrl | MSIX_ENTRY_CTRL_MASK);
    true
}

/// Unmask a specific MSI-X table entry.
///
/// Returns `false` if the entry index is out of range or the table is not mapped.
pub fn msix_unmask_entry(table: &MsixTable, entry_idx: u16) -> bool {
    if entry_idx >= table.table_size || !table.is_mapped() {
        return false;
    }
    let offset = (entry_idx as usize) * MSIX_ENTRY_SIZE + MSIX_ENTRY_VECTOR_CTRL;
    let ctrl = table.table.read::<u32>(offset);
    table
        .table
        .write::<u32>(offset, ctrl & !MSIX_ENTRY_CTRL_MASK);
    true
}

// =============================================================================
// Enable / Disable
// =============================================================================

/// Enable MSI-X for a device and disable legacy INTx.
///
/// Sets the MSI-X enable bit in Message Control and clears the function mask
/// so that individually-unmasked table entries can deliver interrupts.
/// Also disables legacy INTx in the PCI Command register, since the two
/// mechanisms must not be active simultaneously.
///
/// Typical initialization order:
/// 1. Parse capability ([`msix_read_capability`]).
/// 2. Map table ([`msix_map_table`]).
/// 3. Configure entries ([`msix_configure`]).
/// 4. Enable MSI-X ([`msix_enable`]).
pub fn msix_enable(bus: u8, dev: u8, func: u8, cap: &MsixCapability) {
    let cap_off = cap.cap_offset;

    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSIX_REG_CONTROL);
    ctrl |= MSIX_CTRL_ENABLE;
    ctrl &= !MSIX_CTRL_FUNCTION_MASK;
    pci_config_write16(bus, dev, func, cap_off + MSIX_REG_CONTROL, ctrl);

    msi_common::disable_intx(bus, dev, func);

    klog_info!(
        "MSI-X: Enabled for BDF {}:{}.{} ({} entries)",
        bus,
        dev,
        func,
        cap.table_size,
    );
}

/// Disable MSI-X for a device and re-enable legacy INTx.
pub fn msix_disable(bus: u8, dev: u8, func: u8, cap: &MsixCapability) {
    let cap_off = cap.cap_offset;

    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSIX_REG_CONTROL);
    ctrl &= !MSIX_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSIX_REG_CONTROL, ctrl);

    msi_common::restore_intx(bus, dev, func);

    klog_info!("MSI-X: Disabled for BDF {}:{}.{}", bus, dev, func);
}

/// Set the function-level mask for all MSI-X table entries.
///
/// When the function mask is set, no MSI-X interrupts are delivered regardless
/// of individual per-vector mask bits.  Useful for atomic reconfiguration of
/// multiple table entries without spurious interrupts.
pub fn msix_set_function_mask(bus: u8, dev: u8, func: u8, cap: &MsixCapability) {
    let cap_off = cap.cap_offset;
    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSIX_REG_CONTROL);
    ctrl |= MSIX_CTRL_FUNCTION_MASK;
    pci_config_write16(bus, dev, func, cap_off + MSIX_REG_CONTROL, ctrl);
}

/// Clear the function-level mask for all MSI-X table entries.
///
/// After clearing, each table entry's individual mask bit determines whether
/// that entry can deliver interrupts.
pub fn msix_clear_function_mask(bus: u8, dev: u8, func: u8, cap: &MsixCapability) {
    let cap_off = cap.cap_offset;
    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSIX_REG_CONTROL);
    ctrl &= !MSIX_CTRL_FUNCTION_MASK;
    pci_config_write16(bus, dev, func, cap_off + MSIX_REG_CONTROL, ctrl);
}

/// Re-read the Message Control register to refresh capability state.
pub fn msix_refresh_control(bus: u8, dev: u8, func: u8, cap: &mut MsixCapability) {
    cap.control = pci_config_read16(bus, dev, func, cap.cap_offset + MSIX_REG_CONTROL);
}
