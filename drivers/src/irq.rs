use core::ffi::c_void;

use slopos_abi::arch::IRQ_BASE_VECTOR;
use slopos_abi::arch::x86_64::ioapic::{
    IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
    IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_LEVEL,
};
use slopos_core::irq::{
    self, LEGACY_IRQ_COM1, LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE, LEGACY_IRQ_TIMER,
};
use slopos_core::sched::scheduler_timer_tick;
use slopos_lib::{InterruptFrame, cpu, klog_debug, klog_info};

use crate::{apic, ioapic, ps2};

extern "C" fn timer_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    irq::increment_timer_ticks();
    let tick = irq::get_timer_ticks();
    if tick <= 3 {
        klog_debug!("IRQ: Timer tick #{}", tick);
    }
    scheduler_timer_tick();
}

extern "C" fn keyboard_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    if !ps2::has_data() {
        return;
    }
    let scancode = ps2::read_data_nowait();
    irq::increment_keyboard_events();
    ps2::keyboard::handle_scancode(scancode);
}

extern "C" fn mouse_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    if !ps2::is_mouse_data() {
        return;
    }
    let data = ps2::read_data_nowait();
    ps2::mouse::handle_irq(data);
}

fn program_ioapic_route(irq_line: u8) {
    if irq_line as usize >= irq::IRQ_LINES {
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

    irq::set_irq_route(irq_line, gsi);

    let masked = irq::is_masked(irq_line);

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

    program_ioapic_route(LEGACY_IRQ_TIMER);
    program_ioapic_route(LEGACY_IRQ_KEYBOARD);
    program_ioapic_route(LEGACY_IRQ_MOUSE);
    program_ioapic_route(LEGACY_IRQ_COM1);
}

pub fn init() {
    irq::init();

    setup_ioapic_routes();
    ps2::keyboard::init();
    ps2::mouse::init();

    let _ = irq::register_handler(
        LEGACY_IRQ_TIMER,
        Some(timer_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = irq::register_handler(
        LEGACY_IRQ_KEYBOARD,
        Some(keyboard_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = irq::register_handler(
        LEGACY_IRQ_MOUSE,
        Some(mouse_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );

    cpu::enable_interrupts();
}
