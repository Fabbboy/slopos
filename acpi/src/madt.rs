//! MADT (Multiple APIC Description Table) entry iteration over the
//! OSTD `AcpiTable<'a>` slice primitive.

use slopos_ostd::klog_info;
use slopos_ostd::util::packed_view::read_packed;

use crate::tables::{AcpiTable, AcpiTables};

const MADT_SIGNATURE: &[u8; 4] = b"APIC";
const MADT_ENTRY_IOAPIC: u8 = 1;
const MADT_ENTRY_INTERRUPT_OVERRIDE: u8 = 2;

/// PC-AT-compatible dual-8259 present; ACPI 6.5 §5.2.12 requires masking the
/// 8259 vectors before enabling APIC operation when it is set.
pub const MADT_FLAG_PCAT_COMPAT: u32 = 1 << 0;

/// Past the fixed `lapic_address: u32` + `flags: u32`.
const MADT_ENTRIES_OFFSET: usize = 8;

/// `entry_type: u8`, `length: u8`.
const ENTRY_HEADER_SIZE: usize = 2;

const IOAPIC_ENTRY_LEN: usize = 12;
const IOAPIC_OFF_ID: usize = 2;
const IOAPIC_OFF_ADDRESS: usize = 4;
const IOAPIC_OFF_GSI_BASE: usize = 8;

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

/// Override flags bits [1:0], ACPI §5.2.12.5: 0 bus default, 1 active high,
/// 3 active low, 2 reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    BusDefault,
    ActiveHigh,
    ActiveLow,
}

/// Override flags bits [3:2], ACPI §5.2.12.5: 0 bus default, 1 edge, 3 level,
/// 2 reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    BusDefault,
    Edge,
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
    #[inline]
    pub fn polarity(&self) -> Polarity {
        match self.flags & 0x3 {
            0 => Polarity::BusDefault,
            1 => Polarity::ActiveHigh,
            3 => Polarity::ActiveLow,
            _ => Polarity::BusDefault, // reserved → treat as bus default
        }
    }

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

pub struct Madt {
    table: AcpiTable<'static>,
}

impl Madt {
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let Some(table) = tables.find_table(MADT_SIGNATURE) else {
            klog_info!("ACPI: MADT not found");
            return None;
        };
        Self::from_table(table)
    }

    pub fn from_table(table: AcpiTable<'static>) -> Option<Self> {
        if table.signature() != *MADT_SIGNATURE {
            return None;
        }
        if table.payload().len() < MADT_ENTRIES_OFFSET {
            klog_info!("ACPI: MADT too short");
            return None;
        }
        Some(Self { table })
    }

    pub fn flags(&self) -> u32 {
        read_packed::<u32>(self.table.payload(), 4).unwrap_or(0)
    }

    pub fn has_pcat_compat_dual_8259(&self) -> bool {
        self.flags() & MADT_FLAG_PCAT_COMPAT != 0
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
