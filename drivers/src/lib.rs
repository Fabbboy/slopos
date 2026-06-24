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
#[cfg(feature = "test-hooks")]
pub mod tests;
// line_disc is now a submodule of tty/ (drivers/src/tty/ldisc.rs)
pub mod msi;
pub mod msi_common;
pub mod msix;
pub mod pci;
pub mod pci_defs;
pub mod pinctrl;
pub mod pit;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod touchpad;
pub mod tty;
pub mod tty_file_ops;
#[cfg(feature = "test-hooks")]
pub mod tty_tests;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_gpu;
pub mod virtio_net;
#[cfg(feature = "xe-gpu")]
pub mod xe;

pub use driver_core::{BoundDevice, BoundError};
pub use pci::{PciMatch, PciProbeError, ProbeOutcome};
pub use ps2::keyboard;
pub use ps2::mouse;
