use core::ffi::c_void;

use crate::ioapic::regs::{
    IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
    IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_LEVEL,
};
use slopos_arch::arch::idt::IRQ_BASE_VECTOR;
use slopos_arch::{InterruptFrame, cpu};
use slopos_kernel_services::driver_runtime::{
    IRQ_LINES, LEGACY_IRQ_COM1, LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE,
    irq_increment_keyboard_events, irq_init, irq_is_masked, irq_register_handler, irq_set_route,
};
use slopos_utils::klog_info;

use crate::{apic, ioapic, ps2};

// PIT timer IRQ handler and fallback have been removed.
// Scheduler preemption is driven exclusively by the per-CPU LAPIC timer
// (vector LAPIC_TIMER_VECTOR), handled directly in the IDT dispatch —
// see boot/src/idt.rs.  HPET + LAPIC are mandatory.

extern "C" fn ps2_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    let status = ps2::read_status();
    if status & ps2::STATUS_OUTPUT_FULL == 0 {
        return;
    }
    let data = ps2::read_data_nowait();
    match _irq {
        LEGACY_IRQ_KEYBOARD => {
            irq_increment_keyboard_events();
            ps2::keyboard::handle_scancode(data);
        }
        LEGACY_IRQ_MOUSE => {
            ps2::mouse::handle_irq(data);
        }
        _ => {
            if status & ps2::STATUS_MOUSE_DATA != 0 {
                ps2::mouse::handle_irq(data);
            } else {
                irq_increment_keyboard_events();
                ps2::keyboard::handle_scancode(data);
            }
        }
    }
}

fn program_ioapic_route(irq_line: u8) {
    if irq_line as usize >= IRQ_LINES {
        return;
    }

    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        panic!("IRQ: APIC/IOAPIC unavailable during route programming");
    }

    let mut gsi = 0u32;
    let mut legacy_flags = 0u32;
    if ioapic::legacy_irq_info(irq_line, &mut gsi, &mut legacy_flags) != 0 {
        panic!("IRQ: Failed to translate legacy IRQ");
    }

    let vector = IRQ_BASE_VECTOR.wrapping_add(irq_line) as u8;
    let lapic_id = apic::get_id() as u8;
    let flags =
        IOAPIC_FLAG_DELIVERY_FIXED | IOAPIC_FLAG_DEST_PHYSICAL | legacy_flags | IOAPIC_FLAG_MASK;

    if ioapic::config_irq(gsi, vector, lapic_id, flags) != 0 {
        panic!("IRQ: Failed to program IOAPIC route");
    }

    irq_set_route(irq_line, gsi);

    let masked = irq_is_masked(irq_line);

    let polarity = if legacy_flags & IOAPIC_FLAG_POLARITY_LOW != 0 {
        "active-low"
    } else {
        "active-high"
    };
    let trigger = if legacy_flags & IOAPIC_FLAG_TRIGGER_LEVEL != 0 {
        "level"
    } else {
        "edge"
    };

    klog_info!(
        "IRQ: IOAPIC route IRQ {} -> GSI {}, vector 0x{:x} ({}, {})",
        irq_line,
        gsi,
        vector,
        polarity,
        trigger
    );

    if masked {
        let _ = ioapic::mask_gsi(gsi);
    } else {
        let _ = ioapic::unmask_gsi(gsi);
    }
}

fn setup_ioapic_routes() {
    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        panic!("IRQ: APIC/IOAPIC not ready during dispatcher init");
    }

    // PIT timer route removed — scheduler ticks come from the per-CPU LAPIC timer.
    program_ioapic_route(LEGACY_IRQ_KEYBOARD);
    program_ioapic_route(LEGACY_IRQ_MOUSE);
    program_ioapic_route(LEGACY_IRQ_COM1);
}

pub fn init() {
    irq_init();

    setup_ioapic_routes();

    // Full PS/2 controller init: disable ports, flush, self-test, clean config
    ps2::init_controller();

    // Device-level init (controller is ready, IRQs still off)
    ps2::keyboard::init();
    ps2::mouse::init();

    // Final flush before enabling IRQs to drain any stray init response bytes
    ps2::flush();
    // Enable IRQs in the controller config byte now that devices are ready
    ps2::enable_irqs();

    // LAPIC timer handler lives in boot/src/idt.rs (per-CPU, not via IOAPIC).
    let _ = irq_register_handler(
        LEGACY_IRQ_KEYBOARD,
        Some(ps2_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = irq_register_handler(
        LEGACY_IRQ_MOUSE,
        Some(ps2_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );

    cpu::enable_interrupts();
}
