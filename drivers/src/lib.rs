#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod hw;

pub mod apic;
pub mod fate;
pub mod input_event;
pub mod interrupt_test;
pub mod interrupt_test_config;
pub mod interrupts;
pub mod ioapic;
pub mod irq;
pub mod keyboard;
pub mod mouse;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod random;
pub mod scheduler_callbacks;
pub mod serial;
pub mod syscall;
pub mod syscall_common;
pub mod syscall_fs;
pub mod syscall_handlers;
pub mod syscall_types;
pub mod tty;
pub mod video_bridge;
pub mod virtio_gpu;
pub mod wl_currency;
