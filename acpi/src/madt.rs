//! MADT (Multiple APIC Description Table) entry iteration over the
//! OSTD `AcpiTable<'a>` slice primitive.

use slopos_ostd::util::packed_view::read_packed;
use slopos_utils::klog_info;

use crate::tables::{AcpiTable, AcpiTables};

const MADT_SIGNATURE: &[u8; 4] = b"APIC";
const MADT_ENTRY_IOAPIC: u8 = 1;
const MADT_ENTRY_INTERRUPT_OVERRIDE: u8 = 2;

/// Offset of the first variable-length entry within the MADT payload
/// (after `lapic_address: u32` + `flags: u32`).
const MADT_ENTRIES_OFFSET: usize = 8;

/// Per-entry header: `entry_type: u8`, `length: u8`.
const ENTRY_HEADER_SIZE: usize = 2;

/// Layout of the MADT type-1 IOAPIC entry (12 bytes total).
const IOAPIC_ENTRY_LEN: usize = 12;
const IOAPIC_OFF_ID: usize = 2;
const IOAPIC_OFF_ADDRESS: usize = 4;
const IOAPIC_OFF_GSI_BASE: usize = 8;

/// Layout of the MADT type-2 Interrupt Source Override entry (10 bytes total).
const ISO_ENTRY_LEN: usize = 10;
const ISO_OFF_BUS_SOURCE: usize = 2;
const ISO_OFF_IRQ_SOURCE: usize = 3;
const ISO_OFF_GSI: usize = 4;
const ISO_OFF_FLAGS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct IoapicInfo {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

// =============================================================================
// MADT Interrupt Source Override flag parsing
// =============================================================================

/// Polarity of an interrupt source override (MADT flags bits [1:0]).
///
/// Per ACPI spec, §5.2.12.5:
/// - 0b00 = conforms to bus specifications
/// - 0b01 = active high
/// - 0b10 = reserved
/// - 0b11 = active low
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// Conforms to the specifications of the bus.
    BusDefault,
    /// Active high.
    ActiveHigh,
    /// Active low.
    ActiveLow,
}

/// Trigger mode of an interrupt source override (MADT flags bits [3:2]).
///
/// Per ACPI spec, §5.2.12.5:
/// - 0b00 = conforms to bus specifications
/// - 0b01 = edge-triggered
/// - 0b10 = reserved
/// - 0b11 = level-triggered
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    /// Conforms to the specifications of the bus.
    BusDefault,
    /// Edge-triggered.
    Edge,
    /// Level-triggered.
    Level,
}

#[derive(Clone, Copy, Debug)]
pub struct InterruptOverride {
    pub bus_source: u8,
    pub irq_source: u8,
    pub gsi: u32,
    pub flags: u16,
}

impl InterruptOverride {
    /// Parse the polarity field from the MADT flags word.
    #[inline]
    pub fn polarity(&self) -> Polarity {
        match self.flags & 0x3 {
            0 => Polarity::BusDefault,
            1 => Polarity::ActiveHigh,
            3 => Polarity::ActiveLow,
            _ => Polarity::BusDefault, // reserved → treat as bus default
        }
    }

    /// Parse the trigger mode field from the MADT flags word.
    #[inline]
    pub fn trigger_mode(&self) -> TriggerMode {
        match (self.flags >> 2) & 0x3 {
            0 => TriggerMode::BusDefault,
            1 => TriggerMode::Edge,
            3 => TriggerMode::Level,
            _ => TriggerMode::BusDefault, // reserved → treat as bus default
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MadtEntry {
    Ioapic(IoapicInfo),
    InterruptOverride(InterruptOverride),
    Unknown { entry_type: u8 },
}

/// Parsed handle to the MADT, supporting iteration over its entries.
pub struct Madt {
    table: AcpiTable<'static>,
}

impl Madt {
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let Some(table) = tables.find_table(MADT_SIGNATURE) else {
            klog_info!("ACPI: MADT not found");
            return None;
        };
        if table.payload().len() < MADT_ENTRIES_OFFSET {
            klog_info!("ACPI: MADT too short");
            return None;
        }
        Some(Self { table })
    }

    pub fn entries(&self) -> MadtEntries<'_> {
        MadtEntries {
            payload: self.table.payload(),
            cursor: MADT_ENTRIES_OFFSET,
        }
    }
}

pub struct MadtEntries<'a> {
    payload: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for MadtEntries<'a> {
    type Item = MadtEntry;

    fn next(&mut self) -> Option<MadtEntry> {
        let len = self.payload.len();
        if self.cursor + ENTRY_HEADER_SIZE > len {
            return None;
        }
        let entry_type = read_packed::<u8>(self.payload, self.cursor)?;
        let entry_length = read_packed::<u8>(self.payload, self.cursor + 1)? as usize;
        if entry_length == 0 || self.cursor + entry_length > len {
            return None;
        }
        let base = self.cursor;
        self.cursor += entry_length;

        let entry = match entry_type {
            MADT_ENTRY_IOAPIC if entry_length >= IOAPIC_ENTRY_LEN => {
                MadtEntry::Ioapic(IoapicInfo {
                    id: read_packed::<u8>(self.payload, base + IOAPIC_OFF_ID)?,
                    address: read_packed::<u32>(self.payload, base + IOAPIC_OFF_ADDRESS)?,
                    gsi_base: read_packed::<u32>(self.payload, base + IOAPIC_OFF_GSI_BASE)?,
                })
            }
            MADT_ENTRY_INTERRUPT_OVERRIDE if entry_length >= ISO_ENTRY_LEN => {
                MadtEntry::InterruptOverride(InterruptOverride {
                    bus_source: read_packed::<u8>(self.payload, base + ISO_OFF_BUS_SOURCE)?,
                    irq_source: read_packed::<u8>(self.payload, base + ISO_OFF_IRQ_SOURCE)?,
                    gsi: read_packed::<u32>(self.payload, base + ISO_OFF_GSI)?,
                    flags: read_packed::<u16>(self.payload, base + ISO_OFF_FLAGS)?,
                })
            }
            t => MadtEntry::Unknown { entry_type: t },
        };
        Some(entry)
    }
}
