use crate::ioapic::regs::{
    IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
    IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_LEVEL,
};
use slopos_arch::cpu;
use slopos_kernel_services::driver_runtime::{
    IRQ_LINES, LEGACY_IRQ_COM1, LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE, irq_enable_line, irq_init,
    irq_is_masked, irq_set_route,
};
use slopos_ostd::irq::{IRQ_BASE_VECTOR, IrqAllocator, IrqContext};
use slopos_ostd::klog_info;

use crate::{apic, ioapic, ps2};

/// Wire the PS/2 dispatch closure onto a legacy IRQ line, leak both handles for
/// the kernel's lifetime, and unmask the IOAPIC route — `setup_ioapic_routes`
/// programs the RTE masked, so without the unmask the callback is wired but the
/// line stays gated.
fn register_legacy_irq(irq_line: u8) {
    let vector = IRQ_BASE_VECTOR.wrapping_add(irq_line);
    let line = match IrqAllocator::reserve_specific(vector) {
        Ok(l) => l,
        Err(e) => {
            klog_info!("IRQ: reserve_specific(vector={}) failed: {:?}", vector, e);
            return;
        }
    };
    let handle = match line.register_callback(move |_ctx: &IrqContext<'_>| {
        ps2::dispatch_irq(irq_line);
    }) {
        Ok(h) => h,
        Err(e) => {
            klog_info!(
                "IRQ: register_callback for vector {} failed: {:?}",
                vector,
                e
            );
            return;
        }
    };
    // The handle borrows `line`; forget it first to end the borrow.
    core::mem::forget(handle);
    core::mem::forget(line);

    irq_enable_line(irq_line);
}

pub(crate) fn program_ioapic_route(irq_line: u8) {
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

    program_ioapic_route(LEGACY_IRQ_COM1);

    // Otherwise the platform-bus i8042 driver programs and unmasks the PS/2 lines
    // when it binds.
    if ps2::legacy_mode() {
        program_ioapic_route(LEGACY_IRQ_KEYBOARD);
        program_ioapic_route(LEGACY_IRQ_MOUSE);
    }
}

pub fn init() {
    irq_init();

    setup_ioapic_routes();

    // The `i8042.legacy` escape hatch; the default path defers PS/2 bring-up to
    // the platform-bus i8042 driver, which claims 0x60/0x64 + IRQ 1 at probe.
    if ps2::legacy_mode() {
        ps2::init_controller();

        ps2::keyboard::init();
        ps2::mouse::init();

        // Drain stray init responses before the controller starts raising IRQs.
        ps2::flush();
        ps2::enable_irqs();

        register_legacy_irq(LEGACY_IRQ_KEYBOARD);
        register_legacy_irq(LEGACY_IRQ_MOUSE);
    }

    cpu::enable_interrupts();
}
