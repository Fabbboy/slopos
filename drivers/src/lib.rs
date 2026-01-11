#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod fate;
pub mod input_event;
pub mod interrupt_test;
pub mod interrupts;
pub mod ioapic;
pub mod irq;
pub mod keyboard;
pub mod mouse;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod platform_init;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod tty;
pub mod video_bridge;
pub mod virtio_gpu;
// wl_currency moved to slopos_core - re-export for backward compatibility
pub use slopos_core::wl_currency;
