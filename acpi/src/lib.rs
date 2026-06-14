//! ACPI table parsing infrastructure for SlopOS.
//!
//! This crate provides a reusable, zero-allocation ACPI table parser. Any
//! subsystem that needs ACPI data (IOAPIC, HPET, NUMA, PCIe MCFG, etc.)
//! consumes this crate rather than re-implementing table walking.
//!
//! # Architecture
//!
//! - [`tables`]: RSDP validation, XSDT/RSDT traversal, table lookup by signature.
//! - [`madt`]: MADT (Multiple APIC Description Table) entry iteration.
//! - [`hpet`]: HPET (High Precision Event Timer) table parsing.
//! - [`mcfg`]: MCFG (PCI Express ECAM configuration space) table parsing.
//! - [`fadt`]: FADT power-management registers + DSDT `\_S5` decode
//!   (shutdown / reboot).
//!
//! # Usage
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

pub mod fadt;
pub mod hpet;
pub mod madt;
pub mod mcfg;
pub mod tables;
