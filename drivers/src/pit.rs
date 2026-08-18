//! Legacy PIT (Intel 8254) — calibration-only polled delay, the reference for
//! LAPIC timer calibration should the HPET codepath fall through (dead, since
//! HPET is mandatory at boot).
//!
//! Nothing here routes IRQs or configures a frequency: the counter free-runs at
//! its base oscillator frequency (~1.193 182 MHz) from power-on reset.

use slopos_ostd::io::Pit;
use slopos_ostd::io::port::IoPortRegistry;
use slopos_ostd::io::port_consts::PIT_BASE_FREQUENCY_HZ;

/// Hardware default reload value (counter wraps at 0x10000 = 65 536).
const DEFAULT_RELOAD: u32 = 0x10000;

/// Latch and read the PIT channel 0 down-counter. Interrupts are briefly
/// disabled so the two-byte read cannot be split.
fn pit_read_count() -> u16 {
    let pit = Pit::new(
        IoPortRegistry::reserve::<u8>(0x43).expect("PIT command port"),
        IoPortRegistry::reserve::<u8>(0x40).expect("PIT channel 0 port"),
    );
    let flags = slopos_arch::cpu::save_flags_cli();
    let count = pit.read_count();
    slopos_arch::cpu::restore_flags(flags);
    count
}

/// Polled spin-wait for `ms` milliseconds against the free-running channel 0
/// counter, timed from [`PIT_BASE_FREQUENCY_HZ`]. Needs no prior initialisation.
pub fn pit_poll_delay_ms(ms: u32) {
    if ms == 0 {
        return;
    }

    let ticks_needed = ((ms as u64) * (PIT_BASE_FREQUENCY_HZ as u64) / 1000) as u32;
    let mut last = pit_read_count();
    let mut elapsed: u32 = 0;

    while elapsed < ticks_needed {
        core::hint::spin_loop();

        let current = pit_read_count();
        if current <= last {
            elapsed = elapsed.saturating_add((last - current) as u32);
        } else {
            elapsed =
                elapsed.saturating_add(last as u32 + DEFAULT_RELOAD.saturating_sub(current as u32));
        }
        last = current;
    }
}
