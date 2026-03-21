//! HPET (High Precision Event Timer) ACPI table parsing.
//!
//! Discovers the HPET base address and timer block capabilities from the
//! ACPI `"HPET"` table (IA-PC HPET Specification §3.2.4). The driver in
//! [`slopos_drivers::hpet`] consumes this information to map and initialize
//! the HPET MMIO registers.
//!
//! # Usage
//!
//! ```ignore
//! use slopos_acpi::hpet::Hpet;
//! use slopos_acpi::tables::AcpiTables;
//!
//! let tables = AcpiTables::from_rsdp(rsdp_ptr)?;
//! let hpet = Hpet::from_tables(&tables)?;
//! let info = hpet.info();
//! // info.base_phys is the MMIO base address for the HPET register block
//! ```

use core::mem;

use slopos_utils::klog_info;

use crate::tables::{AcpiTables, SdtHeader};

const HPET_SIGNATURE: &[u8; 4] = b"HPET";

// =============================================================================
// Raw ACPI structures (packed, matches hardware layout)
// =============================================================================

/// ACPI Generic Address Structure (GAS).
///
/// Describes a register location — either memory-mapped or port I/O.
/// For HPET, only memory space (`address_space_id == 0`) is valid.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct AcpiGas {
    address_space_id: u8,
    register_bit_width: u8,
    register_bit_offset: u8,
    access_size: u8,
    address: u64,
}

/// Raw ACPI HPET table layout (IA-PC HPET Specification §3.2.4).
///
/// Total size: 56 bytes (36-byte SDT header + 20 bytes HPET-specific).
#[repr(C, packed)]
struct RawHpetTable {
    header: SdtHeader,
    /// Event Timer Block ID:
    ///   Bits [31:16] = PCI Vendor ID
    ///   Bit  [15]    = Legacy Replacement IRQ Routing Capable
    ///   Bit  [13]    = COUNT_SIZE_CAP (1 = 64-bit counter)
    ///   Bits [12:8]  = Number of comparators minus 1
    ///   Bits [7:0]   = Hardware Revision ID
    event_timer_block_id: u32,
    /// Base address of the HPET register block (GAS).
    base_address: AcpiGas,
    /// HPET sequence number (0 for the first HPET).
    hpet_number: u8,
    /// Minimum clock tick in periodic mode.
    minimum_tick: u16,
    /// Page protection and OEM attributes.
    page_protection: u8,
}

// =============================================================================
// Parsed HPET information
// =============================================================================

/// Parsed HPET information extracted from the ACPI table.
///
/// The actual counter tick period (`period_fs`) is not in the ACPI table —
/// it lives in the HPET MMIO capability register and is read by the driver
/// during initialization.
#[derive(Clone, Copy, Debug)]
pub struct HpetInfo {
    /// Physical base address of the HPET MMIO register block.
    pub base_phys: u64,
    /// HPET sequence number (usually 0).
    pub hpet_number: u8,
    /// Number of comparators (timers) available in this timer block.
    pub num_comparators: u8,
    /// Whether the main counter is 64-bit capable.
    pub counter_64bit: bool,
    /// Minimum clock tick value for periodic mode (from the ACPI table).
    pub minimum_tick: u16,
}

/// Parsed handle to the HPET ACPI table.
pub struct Hpet {
    info: HpetInfo,
}

impl Hpet {
    /// Look up the `"HPET"` table in the ACPI hierarchy and parse it.
    ///
    /// Returns `None` if the table is absent, too short, or contains
    /// an invalid address space (only MMIO / memory space 0 is supported).
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let header = tables.find_table(HPET_SIGNATURE);
        if header.is_null() {
            klog_info!("ACPI: HPET table not found");
            return None;
        }

        let length = unsafe { (*header).length } as usize;
        if length < mem::size_of::<RawHpetTable>() {
            klog_info!("ACPI: HPET table too short ({} bytes)", length);
            return None;
        }

        let raw = unsafe { &*(header as *const RawHpetTable) };

        // The base address must reside in memory space (address_space_id == 0).
        // I/O port space is not supported for HPET.
        let addr_space = raw.base_address.address_space_id;
        if addr_space != 0 {
            klog_info!(
                "ACPI: HPET base address in unsupported space ({}), expected memory (0)",
                addr_space
            );
            return None;
        }

        let base_phys = raw.base_address.address;
        if base_phys == 0 {
            klog_info!("ACPI: HPET base address is zero");
            return None;
        }

        let block_id = raw.event_timer_block_id;
        let num_comparators = (((block_id >> 8) & 0x1F) as u8).wrapping_add(1);
        let counter_64bit = (block_id >> 13) & 1 != 0;
        let minimum_tick = raw.minimum_tick;
        let hpet_number = raw.hpet_number;

        Some(Self {
            info: HpetInfo {
                base_phys,
                hpet_number,
                num_comparators,
                counter_64bit,
                minimum_tick,
            },
        })
    }

    /// Return the parsed HPET information.
    #[inline]
    pub fn info(&self) -> HpetInfo {
        self.info
    }
}
