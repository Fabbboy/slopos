#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

use slopos_drivers::{serial_println, wl_currency};

pub fn init() {
    serial_println!("RAMFS is but a dream in Rust for now.");
    wl_currency::award_loss();
}

