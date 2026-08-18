#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

pub mod apic;
pub mod driver_core;
pub mod hpet;
pub mod i2c;
pub mod input_event;
pub mod ioapic;
pub mod irq;
pub mod msi;
pub mod msi_common;
pub mod msix;
pub mod pci;
pub mod pci_defs;
pub mod pinctrl;
pub mod pit;
pub mod platform_bus;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
#[cfg(feature = "test-hooks")]
pub mod tests;
pub mod touchpad;
pub mod tty;
pub mod tty_file_ops;
#[cfg(feature = "test-hooks")]
pub mod tty_tests;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_gpu;
pub mod virtio_net;
pub mod xe;
pub mod xe_logic;

pub use driver_core::{BoundDevice, BoundError};
pub use pci::{PciMatch, PciProbeError, ProbeOutcome};
pub use ps2::keyboard;
pub use ps2::mouse;
