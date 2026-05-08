//! MCFG (PCI Express Memory-Mapped Configuration Space) ACPI table parsing.
//!
//! Discovers PCIe ECAM base addresses and bus ranges from the ACPI `"MCFG"`
//! table (PCI Firmware Specification §4.1.2). The PCI driver in
//! [`slopos_drivers::pci`] consumes this information to enable MMIO-based
//! configuration space access (4 KiB per function, replacing legacy port I/O).

use slopos_ostd::util::packed_view::read_packed;
use slopos_utils::klog_info;

use crate::tables::AcpiTables;

const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// Size of the reserved field at the start of the MCFG payload (after
/// the SDT header).
const MCFG_RESERVED_SIZE: usize = 8;

/// Layout of an MCFG configuration space allocation entry (16 bytes).
const MCFG_ENTRY_LEN: usize = 16;
const MCFG_OFF_BASE: usize = 0;
const MCFG_OFF_SEGMENT: usize = 8;
const MCFG_OFF_BUS_START: usize = 10;
const MCFG_OFF_BUS_END: usize = 11;

/// Maximum number of MCFG entries we track.
const MAX_MCFG_ENTRIES: usize = 16;

/// A single parsed ECAM configuration space allocation entry.
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
    pub fn region_size(&self) -> u64 {
        let bus_count = (self.bus_end as u64) - (self.bus_start as u64) + 1;
        // 256 functions per bus (32 devices × 8 functions) × 4096 bytes each
        bus_count * 256 * 4096
    }

    /// Compute the ECAM MMIO offset for a given BDF within this entry.
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

#[inline(never)]
fn log_mcfg_missing() {
    klog_info!("ACPI: MCFG table not found");
}

#[inline(never)]
fn log_mcfg_too_short(length: usize, min_size: usize) {
    klog_info!(
        "ACPI: MCFG table too short ({} bytes, minimum {})",
        length,
        min_size
    );
}

#[inline(never)]
fn log_mcfg_empty() {
    klog_info!("ACPI: MCFG table present but contains no entries");
}

#[inline(never)]
fn log_mcfg_capped(entry_count: usize, max: usize) {
    klog_info!("ACPI: MCFG has {} entries, capping at {}", entry_count, max);
}

#[inline(never)]
fn log_mcfg_entry_zero(i: usize) {
    klog_info!("ACPI: MCFG entry {} has zero base address, skipping", i);
}

#[inline(never)]
fn log_mcfg_bad_bus_range(i: usize, bus_start: u8, bus_end: u8) {
    klog_info!(
        "ACPI: MCFG entry {} has invalid bus range (start={}, end={}), skipping",
        i,
        bus_start,
        bus_end
    );
}

impl Mcfg {
    /// Look up the `"MCFG"` table in the ACPI hierarchy and parse it.
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let Some(table) = tables.find_table(MCFG_SIGNATURE) else {
            log_mcfg_missing();
            return None;
        };
        let payload = table.payload();
        if payload.len() < MCFG_RESERVED_SIZE {
            log_mcfg_too_short(payload.len(), MCFG_RESERVED_SIZE);
            return None;
        }

        let entry_bytes = payload.len() - MCFG_RESERVED_SIZE;
        let entry_count = entry_bytes / MCFG_ENTRY_LEN;

        let zero_entry = McfgEntry {
            base_phys: 0,
            segment: 0,
            bus_start: 0,
            bus_end: 0,
        };
        let mut entries = [zero_entry; MAX_MCFG_ENTRIES];

        if entry_count == 0 {
            log_mcfg_empty();
            return Some(Self { entries, count: 0 });
        }

        let capped = entry_count.min(MAX_MCFG_ENTRIES);
        if entry_count > MAX_MCFG_ENTRIES {
            log_mcfg_capped(entry_count, MAX_MCFG_ENTRIES);
        }

        let mut written = 0usize;
        for i in 0..capped {
            let off = MCFG_RESERVED_SIZE + i * MCFG_ENTRY_LEN;
            let base = read_packed::<u64>(payload, off + MCFG_OFF_BASE)?;
            let segment = read_packed::<u16>(payload, off + MCFG_OFF_SEGMENT)?;
            let bus_start = read_packed::<u8>(payload, off + MCFG_OFF_BUS_START)?;
            let bus_end = read_packed::<u8>(payload, off + MCFG_OFF_BUS_END)?;

            if base == 0 {
                log_mcfg_entry_zero(i);
                continue;
            }
            if bus_end < bus_start {
                log_mcfg_bad_bus_range(i, bus_start, bus_end);
                continue;
            }

            entries[written] = McfgEntry {
                base_phys: base,
                segment,
                bus_start,
                bus_end,
            };
            written += 1;
        }

        Some(Self {
            entries,
            count: written,
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
    pub fn primary_entry(&self) -> Option<&McfgEntry> {
        self.entries()
            .iter()
            .find(|e| e.segment == 0 && e.base_phys != 0)
    }
}
