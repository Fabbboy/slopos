#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

#[macro_use]
pub mod syscall_macros;
pub mod syscall_context;

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
pub mod syscall;
pub mod syscall_common;
pub mod syscall_fs;
pub mod syscall_handlers;
pub mod syscall_types;
pub mod tty;
pub mod video_bridge;
pub mod virtio_gpu;
// wl_currency moved to slopos_core - re-export for backward compatibility
pub use slopos_core::wl_currency;
