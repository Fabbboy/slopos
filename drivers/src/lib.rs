#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod fate;
pub mod input_event;
pub mod interrupt_test;
pub mod interrupts;
pub mod ioapic;
pub mod irq;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod platform_init;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod tty;
pub mod virtio_blk;
pub mod virtio_gpu;
pub mod xe;

pub use ps2::keyboard;
pub use ps2::mouse;
pub use slopos_core::wl_currency;
