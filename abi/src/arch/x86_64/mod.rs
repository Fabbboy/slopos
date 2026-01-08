//! x86_64 architecture definitions.
//!
//! This module provides type-safe definitions for x86_64 hardware constants,
//! including MSR addresses, GDT selectors, page table flags, and hardware
//! device registers.
//!
//! # Design Philosophy
//!
//! Raw integer constants are wrapped in newtypes to prevent misuse:
//! - `Msr(u32)` for MSR addresses
//! - `SegmentSelector(u16)` for GDT selectors
//! - `Port(u16)` for I/O port addresses
//! - `PageFlags` bitflags for page table entries
//!
//! This provides compile-time safety that raw constants cannot offer.

pub mod apic;
pub mod cpuid;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod memory;
pub mod msr;
pub mod paging;
pub mod pci;
pub mod ports;

// Re-export commonly used types at module level
pub use apic::ApicBaseMsr;
pub use gdt::SegmentSelector;
pub use msr::Msr;
pub use paging::PageFlags;
pub use ports::Port;
