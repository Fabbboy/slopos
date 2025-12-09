#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod fate;
pub mod interrupts;
pub mod ioapic;
pub mod irq;
pub mod keyboard;
pub mod pic;
pub mod random;
pub mod serial;
pub mod wl_currency;

pub use serial::{serial_print, serial_println};
