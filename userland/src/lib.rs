#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

use slopos_drivers::{serial_println, wl_currency};

pub mod roulette;

pub fn init() {
    serial_println!("Userland stubs awaiting real tasks.");
    wl_currency::award_loss();
}

