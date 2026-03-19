#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod apic;
#[cfg(feature = "itests")]
pub mod apic_timer_tests;
#[cfg(feature = "itests")]
pub mod ecam_tests;
pub mod hpet;
#[cfg(feature = "itests")]
pub mod hpet_tests;
pub mod input_event;
pub mod interrupt_test;
pub mod ioapic;
pub mod irq;
// line_disc is now a submodule of tty/ (drivers/src/tty/ldisc.rs)
pub mod msi;
pub mod msix;
#[cfg(feature = "itests")]
pub mod msix_tests;
pub mod pci;
#[cfg(feature = "itests")]
pub mod pci_cap_tests;
pub mod pci_defs;
pub mod pic;
pub mod pit;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod tty;
#[cfg(feature = "itests")]
pub mod tty_tests;
pub mod virtio;
pub mod virtio_blk;
#[cfg(feature = "itests")]
pub mod virtio_completion_tests;
#[cfg(feature = "itests")]
pub mod virtio_msix_tests;
pub mod virtio_net;
#[cfg(feature = "itests")]
pub mod virtio_net_tests;
#[cfg(feature = "xe-gpu")]
pub mod xe;

pub use ps2::keyboard;
pub use ps2::mouse;
