#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

use slopos_drivers::{serial_println, wl_currency};
use slopos_lib::cpu;

pub struct Scheduler;

impl Scheduler {
    pub fn init() {
        serial_println!(
            "Scheduler waking with W/L balance {}",
            wl_currency::check_balance()
        );
        wl_currency::award_win();
    }

    pub fn idle() -> ! {
        serial_println!("Scheduler entering idle loop. Fate is patient.");
        cpu::halt_loop();
    }
}

