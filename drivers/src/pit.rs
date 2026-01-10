use core::sync::atomic::{AtomicU32, Ordering};

use slopos_lib::{cpu, io, klog_debug, klog_info};

use crate::irq;
use slopos_abi::arch::x86_64::ports::{
    PIT_BASE_FREQUENCY_HZ, PIT_CHANNEL0_PORT, PIT_COMMAND_ACCESS_LOHI, PIT_COMMAND_BINARY,
    PIT_COMMAND_CHANNEL0, PIT_COMMAND_MODE_SQUARE, PIT_COMMAND_PORT, PIT_DEFAULT_FREQUENCY_HZ,
    PIT_IRQ_LINE,
};

static CURRENT_FREQUENCY_HZ: AtomicU32 = AtomicU32::new(0);
static CURRENT_RELOAD_DIVISOR: AtomicU32 = AtomicU32::new(0);

#[inline]
fn pit_io_wait() {
    unsafe {
        io::outb(0x80, 0);
    }
}

fn pit_calculate_divisor(mut frequency_hz: u32) -> u16 {
    if frequency_hz == 0 {
        frequency_hz = PIT_DEFAULT_FREQUENCY_HZ;
    }
    if frequency_hz > PIT_BASE_FREQUENCY_HZ {
        frequency_hz = PIT_BASE_FREQUENCY_HZ;
    }

    let mut divisor = PIT_BASE_FREQUENCY_HZ / frequency_hz;
    if divisor == 0 {
        divisor = 1;
    } else if divisor > 0xFFFF {
        divisor = 0xFFFF;
    }

    let actual_freq = PIT_BASE_FREQUENCY_HZ / divisor;
    CURRENT_FREQUENCY_HZ.store(actual_freq, Ordering::SeqCst);
    CURRENT_RELOAD_DIVISOR.store(divisor, Ordering::SeqCst);
    divisor as u16
}
pub fn pit_set_frequency(frequency_hz: u32) {
    let divisor = pit_calculate_divisor(frequency_hz);

    unsafe {
        io::outb(
            PIT_COMMAND_PORT,
            PIT_COMMAND_CHANNEL0
                | PIT_COMMAND_ACCESS_LOHI
                | PIT_COMMAND_MODE_SQUARE
                | PIT_COMMAND_BINARY,
        );
        io::outb(PIT_CHANNEL0_PORT, (divisor & 0xFF) as u8);
        io::outb(PIT_CHANNEL0_PORT, ((divisor >> 8) & 0xFF) as u8);
    }
    pit_io_wait();

    let freq = CURRENT_FREQUENCY_HZ.load(Ordering::SeqCst);
    klog_debug!("PIT: frequency set to {} Hz\n", freq);
}
pub fn pit_init(frequency_hz: u32) {
    let freq = if frequency_hz == 0 {
        PIT_DEFAULT_FREQUENCY_HZ
    } else {
        frequency_hz
    };
    klog_info!("PIT: Initializing timer at {} Hz\n", freq);
    pit_set_frequency(freq);
    // Leave IRQ masked until scheduler enables preemption.
}
pub fn pit_get_frequency() -> u32 {
    let freq = CURRENT_FREQUENCY_HZ.load(Ordering::SeqCst);
    if freq == 0 {
        PIT_DEFAULT_FREQUENCY_HZ
    } else {
        freq
    }
}
pub fn pit_enable_irq() {
    irq::enable_line(PIT_IRQ_LINE);
}
pub fn pit_disable_irq() {
    irq::disable_line(PIT_IRQ_LINE);
}

fn pit_read_count() -> u16 {
    unsafe {
        io::outb(PIT_COMMAND_PORT, 0x00);
        let low = io::inb(PIT_CHANNEL0_PORT);
        let high = io::inb(PIT_CHANNEL0_PORT);
        ((high as u16) << 8) | (low as u16)
    }
}
pub fn pit_poll_delay_ms(ms: u32) {
    if ms == 0 {
        return;
    }

    let reload = {
        let d = CURRENT_RELOAD_DIVISOR.load(Ordering::SeqCst);
        if d == 0 { 0x10000 } else { d }
    };

    // Calculate ticks needed in raw PIT frequency (1.193182 MHz).
    // The pit_read_count() returns raw PIT counter values, so we must use
    // PIT_BASE_FREQUENCY_HZ, not the configured timer frequency.
    let ticks_needed = ((ms as u64) * (PIT_BASE_FREQUENCY_HZ as u64) / 1000) as u32;
    let mut last = pit_read_count();
    let mut elapsed: u32 = 0;

    while elapsed < ticks_needed {
        // Prevent the compiler from optimizing this loop
        core::hint::spin_loop();

        let current = pit_read_count();
        if current <= last {
            elapsed = elapsed.saturating_add((last - current) as u32);
        } else {
            elapsed = elapsed.saturating_add(last as u32 + (reload.saturating_sub(current as u32)));
        }
        last = current;
    }
}
pub fn pit_sleep_ms(ms: u32) {
    if ms == 0 {
        return;
    }
    let freq = pit_get_frequency();
    let mut ticks_needed = (ms as u64 * freq as u64) / 1000;
    if ticks_needed == 0 {
        ticks_needed = 1;
    }

    let start = irq::get_timer_ticks();
    let target = start.wrapping_add(ticks_needed);

    while irq::get_timer_ticks() < target {
        cpu::hlt();
    }
}
