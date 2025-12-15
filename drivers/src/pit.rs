use core::sync::atomic::{AtomicU32, Ordering};

use slopos_lib::{cpu, io, klog_debug, klog_info};

use crate::irq;

pub const PIT_BASE_FREQUENCY_HZ: u32 = 1_193_182;
pub const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

const PIT_CHANNEL0_PORT: u16 = 0x40;
const PIT_COMMAND_PORT: u16 = 0x43;

const PIT_COMMAND_CHANNEL0: u8 = 0x00;
const PIT_COMMAND_ACCESS_LOHI: u8 = 0x30;
const PIT_COMMAND_MODE_SQUARE: u8 = 0x06;
const PIT_COMMAND_BINARY: u8 = 0x00;

const PIT_IRQ_LINE: u8 = 0;

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

#[unsafe(no_mangle)]
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

#[unsafe(no_mangle)]
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

#[unsafe(no_mangle)]
pub fn pit_get_frequency() -> u32 {
    let freq = CURRENT_FREQUENCY_HZ.load(Ordering::SeqCst);
    if freq == 0 {
        PIT_DEFAULT_FREQUENCY_HZ
    } else {
        freq
    }
}

#[unsafe(no_mangle)]
pub fn pit_enable_irq() {
    irq::enable_line(PIT_IRQ_LINE);
}

#[unsafe(no_mangle)]
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

#[unsafe(no_mangle)]
pub fn pit_poll_delay_ms(ms: u32) {
    if ms == 0 {
        return;
    }

    let freq = {
        let f = CURRENT_FREQUENCY_HZ.load(Ordering::SeqCst);
        if f == 0 { PIT_DEFAULT_FREQUENCY_HZ } else { f }
    };
    let reload = {
        let d = CURRENT_RELOAD_DIVISOR.load(Ordering::SeqCst);
        if d == 0 { 0x10000 } else { d }
    };

    let ticks_needed = ((ms as u64) * freq as u64 / 1000) as u32;
    let mut last = pit_read_count();
    let mut elapsed: u32 = 0;

    while elapsed < ticks_needed {
        let current = pit_read_count();
        if current <= last {
            elapsed = elapsed.saturating_add((last - current) as u32);
        } else {
            elapsed = elapsed.saturating_add(last as u32 + (reload.saturating_sub(current as u32)));
        }
        last = current;
    }
}

#[unsafe(no_mangle)]
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
