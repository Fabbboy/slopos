#![no_std]
#![feature(allocator_api)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod hpet;
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
pub mod pic;
pub mod pit;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod tty;
pub mod tty_file_ops;
#[cfg(feature = "test-hooks")]
pub mod tty_tests;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_net;
#[cfg(feature = "xe-gpu")]
pub mod xe;

pub use ps2::keyboard;
pub use ps2::mouse;
