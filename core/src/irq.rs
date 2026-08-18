//! IRQ surface for the kernel side.
//!
//! Re-exports the OSTD types drivers consume, and keeps the kernel-internal
//! timer/keyboard counters plus the IOAPIC route and mask book-keeping that
//! `drivers/src/irq.rs` queries during boot. OSTD's
//! [`slopos_ostd::irq::dispatch`] remains the single dispatch path for
//! vectors ≥ 32.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

pub use slopos_kernel_services::driver_runtime::IRQ_LINES;
pub use slopos_ostd::irq::*;

use slopos_kernel_services::platform;

/// Incremented from the PS/2 keyboard dispatch closure in `drivers/src/irq.rs`.
static KEYBOARD_EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn get_timer_ticks() -> u64 {
    slopos_kernel_services::clock::get_timer_ticks()
}

#[inline]
pub fn increment_timer_ticks() {
    slopos_kernel_services::clock::increment_timer_ticks();
}

#[inline]
pub fn get_keyboard_event_counter() -> u64 {
    KEYBOARD_EVENT_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_keyboard_events() {
    KEYBOARD_EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
}

// `drivers/src/irq.rs::program_ioapic_route` stores the (line, gsi) mapping
// here so later mask/unmask requests can flush to the IOAPIC.
static IRQ_GSI: [AtomicU32; IRQ_LINES] = [const { AtomicU32::new(0) }; IRQ_LINES];

/// Per-IRQ-line bitmap: bit 0 = via_ioapic, bit 1 = masked.
static IRQ_FLAGS: [AtomicU8; IRQ_LINES] = [const { AtomicU8::new(FLAG_MASKED) }; IRQ_LINES];

const FLAG_VIA_IOAPIC: u8 = 1 << 0;
const FLAG_MASKED: u8 = 1 << 1;

#[derive(Clone, Copy)]
pub struct IrqRouteState {
    pub via_ioapic: bool,
    pub gsi: u32,
}

#[inline]
fn flags_get(irq: u8) -> u8 {
    IRQ_FLAGS[irq as usize].load(Ordering::Acquire)
}

#[inline]
fn flags_set_bits(irq: u8, mask: u8) {
    IRQ_FLAGS[irq as usize].fetch_or(mask, Ordering::AcqRel);
}

#[inline]
fn flags_clear_bits(irq: u8, mask: u8) {
    IRQ_FLAGS[irq as usize].fetch_and(!mask, Ordering::AcqRel);
}

/// Initialise the IRQ book-keeping arrays. Idempotent.
pub fn init() {
    for i in 0..IRQ_LINES {
        IRQ_GSI[i].store(0, Ordering::Release);
        IRQ_FLAGS[i].store(FLAG_MASKED, Ordering::Release);
    }
    slopos_kernel_services::clock::reset_timer_ticks();
    KEYBOARD_EVENT_COUNTER.store(0, Ordering::Relaxed);
}

#[inline]
pub fn is_initialized() -> bool {
    // OSTD's IrqAllocator and DISPATCH table are always available.
    true
}

pub fn set_irq_route(irq: u8, gsi: u32) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    IRQ_GSI[irq as usize].store(gsi, Ordering::Release);
    flags_set_bits(irq, FLAG_VIA_IOAPIC);
}

pub fn get_irq_route(irq: u8) -> Option<IrqRouteState> {
    if irq as usize >= IRQ_LINES {
        return None;
    }
    let f = flags_get(irq);
    Some(IrqRouteState {
        via_ioapic: f & FLAG_VIA_IOAPIC != 0,
        gsi: IRQ_GSI[irq as usize].load(Ordering::Acquire),
    })
}

/// Book-keeping state only; the IOAPIC may differ if a driver bypassed this
/// surface.
pub fn is_masked(irq: u8) -> bool {
    if irq as usize >= IRQ_LINES {
        return true;
    }
    flags_get(irq) & FLAG_MASKED != 0
}

pub fn mask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    let f = flags_get(irq);
    if f & FLAG_MASKED != 0 {
        return;
    }
    flags_set_bits(irq, FLAG_MASKED);
    if f & FLAG_VIA_IOAPIC != 0 {
        let gsi = IRQ_GSI[irq as usize].load(Ordering::Acquire);
        platform::irq_mask_gsi(gsi);
    }
}

pub fn unmask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    let f = flags_get(irq);
    if f & FLAG_MASKED == 0 {
        return;
    }
    flags_clear_bits(irq, FLAG_MASKED);
    if f & FLAG_VIA_IOAPIC != 0 {
        let gsi = IRQ_GSI[irq as usize].load(Ordering::Acquire);
        platform::irq_unmask_gsi(gsi);
    }
}

pub fn enable_line(irq: u8) {
    unmask_irq_line(irq);
}

pub fn disable_line(irq: u8) {
    mask_irq_line(irq);
}
