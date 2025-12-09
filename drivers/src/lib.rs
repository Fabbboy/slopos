#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod interrupts;
pub mod random;
pub mod serial;
pub mod fate;
pub mod wl_currency;

pub use serial::{serial_print, serial_println};

