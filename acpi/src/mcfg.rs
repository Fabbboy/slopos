//! MCFG (PCI Express Memory-Mapped Configuration Space) ACPI table parsing.
//!
//! Discovers PCIe ECAM base addresses and bus ranges from the ACPI `"MCFG"`
//! table (PCI Firmware Specification §4.1.2).  The PCI driver in
//! [`slopos_drivers::pci`] consumes this information to enable MMIO-based
//! configuration space access (4 KiB per function, replacing legacy port I/O).
//!
//! # Usage
//!
//! ```ignore
//! use slopos_acpi::mcfg::Mcfg;
//! use slopos_acpi::tables::AcpiTables;
//!
//! let tables = AcpiTables::from_rsdp(rsdp_ptr)?;
//! let mcfg = Mcfg::from_tables(&tables)?;
//! for entry in mcfg.entries() {
//!     // entry.base_phys is the ECAM MMIO base for this segment/bus range
//! }
//! ```

use core::mem;

use slopos_utils::klog_info;

use crate::tables::{AcpiTables, SdtHeader};

const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// Size of the reserved field between the SDT header and the first entry.
const MCFG_RESERVED_SIZE: usize = 8;

// =============================================================================
// Raw ACPI structures (packed, matches hardware layout)
// =============================================================================

/// Raw ACPI MCFG table layout (PCI Firmware Specification §4.1.2).
///
/// Total header size: 36-byte SDT header + 8 bytes reserved = 44 bytes.
/// Followed by a variable-length array of 16-byte allocation entries.
#[repr(C, packed)]
struct RawMcfgTable {
    header: SdtHeader,
    /// Reserved — must be zero per spec.
    _reserved: [u8; MCFG_RESERVED_SIZE],
    // Followed by: RawMcfgEntry[]
}

/// Raw MCFG configuration space base address allocation entry (16 bytes).
///
/// Each entry describes one PCI segment group's ECAM region.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RawMcfgEntry {
    /// Physical base address of the ECAM region for this segment/bus range.
    base_address: u64,
    /// PCI segment group number.
    segment_group: u16,
    /// Start PCI bus number decoded by this entry.
    start_bus: u8,
    /// End PCI bus number decoded by this entry (inclusive).
    end_bus: u8,
    /// Reserved — must be zero.
    _reserved: u32,
}

// =============================================================================
// Parsed MCFG information
// =============================================================================

/// Maximum number of MCFG entries we track.
///
/// Most systems have exactly 1 entry (segment 0, buses 0–255).  16 is generous
/// enough to cover multi-segment servers while keeping stack allocation bounded.
const MAX_MCFG_ENTRIES: usize = 16;

/// A single parsed ECAM configuration space allocation entry.
///
/// Describes the MMIO region for one PCI segment group's bus range.
/// The ECAM address for a specific function is:
///   `base_phys + ((bus - bus_start) << 20) | (device << 15) | (function << 12) | reg_offset`
#[derive(Clone, Copy, Debug)]
pub struct McfgEntry {
    /// Physical base address of the ECAM MMIO region.
    pub base_phys: u64,
    /// PCI segment group number (usually 0).
    pub segment: u16,
    /// First PCI bus number in this entry's range.
    pub bus_start: u8,
    /// Last PCI bus number in this entry's range (inclusive).
    pub bus_end: u8,
}

impl McfgEntry {
    /// Compute the total MMIO region size in bytes for this entry.
    ///
    /// Each bus has 32 devices × 8 functions × 4 KiB = 1 MiB.
    /// Total = (bus_end - bus_start + 1) × 256 × 4096 bytes.
    pub fn region_size(&self) -> u64 {
        let bus_count = (self.bus_end as u64) - (self.bus_start as u64) + 1;
        // 256 functions per bus (32 devices × 8 functions) × 4096 bytes each
        bus_count * 256 * 4096
    }

    /// Compute the ECAM MMIO offset for a given BDF within this entry.
    ///
    /// Returns `None` if the bus is outside this entry's range.
    pub fn ecam_offset(&self, bus: u8, device: u8, function: u8) -> Option<u64> {
        if bus < self.bus_start || bus > self.bus_end {
            return None;
        }
        if device >= 32 || function >= 8 {
            return None;
        }
        let relative_bus = (bus - self.bus_start) as u64;
        Some((relative_bus << 20) | ((device as u64) << 15) | ((function as u64) << 12))
    }
}

/// Parsed MCFG table containing all discovered ECAM entries.
pub struct Mcfg {
    entries: [McfgEntry; MAX_MCFG_ENTRIES],
    count: usize,
}

impl Mcfg {
    /// Look up the `"MCFG"` table in the ACPI hierarchy and parse it.
    ///
    /// Returns `None` if the table is absent or too short.  An empty table
    /// (header present but zero entries) returns `Some` with `count() == 0`.
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let header = tables.find_table(MCFG_SIGNATURE);
        if header.is_null() {
            klog_info!("ACPI: MCFG table not found");
            return None;
        }

        let length = unsafe { (*header).length } as usize;
        let min_size = mem::size_of::<RawMcfgTable>();
        if length < min_size {
            klog_info!(
                "ACPI: MCFG table too short ({} bytes, minimum {})",
                length,
                min_size
            );
            return None;
        }

        // Compute number of allocation entries after the fixed header.
        let entry_bytes = length - min_size;
        let entry_size = mem::size_of::<RawMcfgEntry>();
        let entry_count = entry_bytes / entry_size;

        if entry_count == 0 {
            klog_info!("ACPI: MCFG table present but contains no entries");
            return Some(Self {
                entries: [McfgEntry {
                    base_phys: 0,
                    segment: 0,
                    bus_start: 0,
                    bus_end: 0,
                }; MAX_MCFG_ENTRIES],
                count: 0,
            });
        }

        let capped = entry_count.min(MAX_MCFG_ENTRIES);
        if entry_count > MAX_MCFG_ENTRIES {
            klog_info!(
                "ACPI: MCFG has {} entries, capping at {}",
                entry_count,
                MAX_MCFG_ENTRIES
            );
        }

        let entries_base = (header as *const u8).wrapping_add(min_size) as *const RawMcfgEntry;

        let mut entries = [McfgEntry {
            base_phys: 0,
            segment: 0,
            bus_start: 0,
            bus_end: 0,
        }; MAX_MCFG_ENTRIES];

        for i in 0..capped {
            let raw = unsafe { &*entries_base.add(i) };
            let base = raw.base_address;
            let segment = raw.segment_group;
            let bus_start = raw.start_bus;
            let bus_end = raw.end_bus;

            // Sanity: base address must be non-zero and bus range valid.
            if base == 0 {
                klog_info!("ACPI: MCFG entry {} has zero base address, skipping", i);
                continue;
            }
            if bus_end < bus_start {
                klog_info!(
                    "ACPI: MCFG entry {} has invalid bus range (start={}, end={}), skipping",
                    i,
                    bus_start,
                    bus_end
                );
                continue;
            }

            entries[i] = McfgEntry {
                base_phys: base,
                segment,
                bus_start,
                bus_end,
            };
        }

        Some(Self {
            entries,
            count: capped,
        })
    }

    /// Number of valid ECAM entries.
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Iterate over the parsed ECAM entries.
    pub fn entries(&self) -> &[McfgEntry] {
        &self.entries[..self.count]
    }

    /// Find the MCFG entry covering a given PCI segment and bus number.
    pub fn find_entry(&self, segment: u16, bus: u8) -> Option<&McfgEntry> {
        self.entries().iter().find(|e| {
            e.segment == segment && bus >= e.bus_start && bus <= e.bus_end && e.base_phys != 0
        })
    }

    /// Find the ECAM entry for segment 0 (the primary/only segment on most systems).
    ///
    /// This is a convenience for the common single-segment case.
    pub fn primary_entry(&self) -> Option<&McfgEntry> {
        self.entries()
            .iter()
            .find(|e| e.segment == 0 && e.base_phys != 0)
    }
}
