//! Zero-allocation ACPI table parsing: RSDP validation, XSDT/RSDT traversal
//! and decoding of the MADT, HPET, MCFG and FADT.
//!
//! ```ignore
//! use slopos_acpi::tables::AcpiTables;
//! use slopos_acpi::madt::{MadtEntries, MadtEntry};
//!
//! let tables = AcpiTables::from_rsdp(rsdp_ptr)?;
//! let madt = tables.find_madt()?;
//!
//! for entry in madt.entries() {
//!     match entry {
//!         MadtEntry::Ioapic(info) => { /* configure IOAPIC */ }
//!         MadtEntry::InterruptOverride(iso) => { /* record override */ }
//!         _ => {}
//!     }
//! }
//! ```

#![no_std]
#![forbid(unsafe_code)]

pub mod aml;
pub mod fadt;
pub mod hpet;
pub mod madt;
pub mod mcfg;
pub mod tables;
