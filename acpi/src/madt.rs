use core::mem;

use slopos_utils::klog_info;

use crate::tables::{AcpiTables, SdtHeader};

const MADT_SIGNATURE: &[u8; 4] = b"APIC";
const MADT_ENTRY_IOAPIC: u8 = 1;
const MADT_ENTRY_INTERRUPT_OVERRIDE: u8 = 2;

#[repr(C, packed)]
struct RawMadt {
    header: SdtHeader,
    lapic_address: u32,
    flags: u32,
    entries: [u8; 0],
}

#[repr(C, packed)]
struct RawEntryHeader {
    entry_type: u8,
    length: u8,
}

#[repr(C, packed)]
struct RawIoapicEntry {
    header: RawEntryHeader,
    ioapic_id: u8,
    reserved: u8,
    ioapic_address: u32,
    gsi_base: u32,
}

#[repr(C, packed)]
struct RawIsoEntry {
    header: RawEntryHeader,
    bus_source: u8,
    irq_source: u8,
    gsi: u32,
    flags: u16,
}

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
    base: *const u8,
    total_length: usize,
}

impl Madt {
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let header = tables.find_table(MADT_SIGNATURE);
        if header.is_null() {
            klog_info!("ACPI: MADT not found");
            return None;
        }
        let length = unsafe { (*header).length } as usize;
        if length < mem::size_of::<RawMadt>() {
            klog_info!("ACPI: MADT too short");
            return None;
        }
        Some(Self {
            base: header as *const u8,
            total_length: length,
        })
    }

    pub fn entries(&self) -> MadtEntries<'_> {
        let entries_offset = mem::size_of::<RawMadt>();
        MadtEntries {
            _madt: self,
            ptr: unsafe { self.base.add(entries_offset) },
            end: unsafe { self.base.add(self.total_length) },
        }
    }
}

pub struct MadtEntries<'a> {
    _madt: &'a Madt,
    ptr: *const u8,
    end: *const u8,
}

impl<'a> Iterator for MadtEntries<'a> {
    type Item = MadtEntry;

    fn next(&mut self) -> Option<MadtEntry> {
        loop {
            let header_end = unsafe { self.ptr.add(mem::size_of::<RawEntryHeader>()) };
            if header_end > self.end {
                return None;
            }

            let hdr = unsafe { &*(self.ptr as *const RawEntryHeader) };
            if hdr.length == 0 {
                return None;
            }
            let entry_end = unsafe { self.ptr.add(hdr.length as usize) };
            if entry_end > self.end {
                return None;
            }

            let entry = match hdr.entry_type {
                MADT_ENTRY_IOAPIC if hdr.length as usize >= mem::size_of::<RawIoapicEntry>() => {
                    let raw = unsafe { &*(self.ptr as *const RawIoapicEntry) };
                    MadtEntry::Ioapic(IoapicInfo {
                        id: raw.ioapic_id,
                        address: raw.ioapic_address,
                        gsi_base: raw.gsi_base,
                    })
                }
                MADT_ENTRY_INTERRUPT_OVERRIDE
                    if hdr.length as usize >= mem::size_of::<RawIsoEntry>() =>
                {
                    let raw = unsafe { &*(self.ptr as *const RawIsoEntry) };
                    MadtEntry::InterruptOverride(InterruptOverride {
                        bus_source: raw.bus_source,
                        irq_source: raw.irq_source,
                        gsi: raw.gsi,
                        flags: raw.flags,
                    })
                }
                t => MadtEntry::Unknown { entry_type: t },
            };

            self.ptr = entry_end;
            return Some(entry);
        }
    }
}
