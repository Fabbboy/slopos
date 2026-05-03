//! IRQ surface for the kernel side.
//!
//! The OSTD-supplied vector allocator and dispatch table own the per-vector
//! callbacks; this module re-exports the OSTD types/functions that drivers
//! consume and keeps the small handful of kernel-internal pieces that never
//! belonged in OSTD:
//!
//!   - timer / keyboard event counters (policy state read by tests + diagnostics)
//!   - the legacy IOAPIC route / mask book-keeping that drivers/src/irq.rs
//!     queries during boot (per-IRQ-line mask state and GSI mapping)
//!
//! Both halves are wrappers around module-local statics with no IRQ-table
//! data structure of their own; OSTD's [`slopos_ostd::irq::dispatch`] is the
//! single dispatch path for vectors ≥ 32.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

pub use slopos_kernel_services::driver_runtime::IRQ_LINES;
pub use slopos_ostd::irq::*;

use slopos_kernel_services::platform;

// =============================================================================
// Timer + keyboard event counters
// =============================================================================

/// Global timer tick counter, incremented from the LAPIC timer arm in
/// `boot/src/idt.rs::common_exception_handler_impl`. Relaxed because tests
/// only need eventual consistency.
static TIMER_TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Global keyboard event counter, incremented from the PS/2 keyboard
/// dispatch closure in `drivers/src/irq.rs`.
static KEYBOARD_EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn get_timer_ticks() -> u64 {
    TIMER_TICK_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_timer_ticks() {
    TIMER_TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn get_keyboard_event_counter() -> u64 {
    KEYBOARD_EVENT_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_keyboard_events() {
    KEYBOARD_EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
}

// =============================================================================
// Per-IRQ-line route / mask book-keeping (legacy IOAPIC bridge)
// =============================================================================
//
// Drivers/src/irq.rs::program_ioapic_route stores the (line, gsi) mapping
// here so subsequent mask/unmask requests can flush to the IOAPIC via the
// platform service. The state is plain atomics indexed by IRQ line — there
// is no entry struct, no callback storage, and no lock.

static IRQ_GSI: [AtomicU32; IRQ_LINES] = [const { AtomicU32::new(0) }; IRQ_LINES];

/// Per-IRQ-line bitmap: bit 0 = via_ioapic, bit 1 = masked.
static IRQ_FLAGS: [AtomicU8; IRQ_LINES] = [const { AtomicU8::new(FLAG_MASKED) }; IRQ_LINES];

const FLAG_VIA_IOAPIC: u8 = 1 << 0;
const FLAG_MASKED: u8 = 1 << 1;

/// IOAPIC route record returned by [`get_irq_route`].
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
    TIMER_TICK_COUNTER.store(0, Ordering::Relaxed);
    KEYBOARD_EVENT_COUNTER.store(0, Ordering::Relaxed);
}

#[inline]
pub fn is_initialized() -> bool {
    // OSTD's IrqAllocator + DISPATCH table is always available; the
    // legacy "framework initialised" flag is no longer load-bearing.
    true
}

/// Record the IOAPIC GSI mapping for an IRQ line.
pub fn set_irq_route(irq: u8, gsi: u32) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    IRQ_GSI[irq as usize].store(gsi, Ordering::Release);
    flags_set_bits(irq, FLAG_VIA_IOAPIC);
}

/// Read the IOAPIC GSI mapping for an IRQ line.
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

/// Whether the line is currently masked (in our book-keeping; the IOAPIC
/// hardware state may differ if a driver bypassed this surface).
pub fn is_masked(irq: u8) -> bool {
    if irq as usize >= IRQ_LINES {
        return true;
    }
    flags_get(irq) & FLAG_MASKED != 0
}

/// Mask an IRQ line at the IOAPIC and update the local flag.
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

/// Unmask an IRQ line at the IOAPIC and update the local flag.
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

/// Enable: clears any "unhandled" bookkeeping (no-op now) and unmasks.
pub fn enable_line(irq: u8) {
    unmask_irq_line(irq);
}

/// Disable: masks the line.
pub fn disable_line(irq: u8) {
    mask_irq_line(irq);
}
