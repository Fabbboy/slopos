use crate::{random::Lfsr64, serial_println, wl_currency};
use slopos_lib::cpu;

pub enum RouletteOutcome {
    Survive,
    Panic,
}

pub struct Wheel {
    rng: Lfsr64,
}

impl Wheel {
    pub fn new() -> Self {
        Self {
            rng: Lfsr64::from_tsc(),
        }
    }

    pub fn spin(&mut self) -> RouletteOutcome {
        let roll = self.rng.next();
        serial_println!("=== KERNEL ROULETTE: Spinning the Wheel of Fate ===");
        serial_println!("Random number: 0x{:016x}", roll);
        if roll & 1 == 0 {
            wl_currency::award_loss();
            serial_println!("Even number. The wheel has spoken. Destiny awaits in the abyss.");
            RouletteOutcome::Panic
        } else {
            wl_currency::award_win();
            serial_println!("Odd number. The wizards live to gamble another boot.");
            RouletteOutcome::Survive
        }
    }
}

pub fn detonate() -> ! {
    serial_println!("=== INITIATING KERNEL PANIC (ROULETTE RESULT) ===");
    loop {
        cpu::hlt();
    }
}

