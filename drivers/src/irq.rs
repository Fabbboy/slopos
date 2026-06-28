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

// PIT timer IRQ handler and fallback have been removed.
// Scheduler preemption is driven exclusively by the per-CPU LAPIC timer
// (vector LAPIC_TIMER_VECTOR), handled directly in the IDT dispatch —
// see boot/src/idt.rs.  HPET + LAPIC are mandatory.

/// Reserve the IDT vector for a hardware-pinned legacy IRQ line, register
/// the PS/2 dispatch closure, leak both handles so the registration
/// persists for the kernel's lifetime, and unmask the IOAPIC route so
/// the line actually fires.
///
/// `setup_ioapic_routes` programs the IOAPIC RTE with the mask bit set
/// (matching the default `FLAG_MASKED` state in `core::irq`'s book-keeping).
/// Without the `irq_enable_line` call below the OSTD callback is wired but
/// the IOAPIC keeps the line gated, so PS/2 input never reaches us.
///
/// Used only by the `i8042.legacy` escape-hatch bring-up below; the default
/// path wires the PS/2 IRQs through the platform-bus i8042 driver's
/// `request_legacy_irq` (see `crate::ps2::platform`).
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
    // Order matters for the borrow checker: the handle borrows `line`, so we
    // forget the handle (ending the borrow) before forgetting the line.
    core::mem::forget(handle);
    core::mem::forget(line);

    // Now that the OSTD dispatch slot is populated, unmask the IOAPIC RTE
    // so the line fires.  This mirrors the legacy `register_handler` path
    // which always called `unmask_irq_line` at the end.
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

    // PIT timer route removed — scheduler ticks come from the per-CPU LAPIC timer.
    program_ioapic_route(LEGACY_IRQ_COM1);

    // The default path leaves the PS/2 lines masked here; the platform-bus
    // i8042 driver programs + unmasks them when it binds (priority-80 PCI
    // probe). The legacy escape hatch routes them up front instead.
    if ps2::legacy_mode() {
        program_ioapic_route(LEGACY_IRQ_KEYBOARD);
        program_ioapic_route(LEGACY_IRQ_MOUSE);
    }
}

pub fn init() {
    irq_init();

    setup_ioapic_routes();

    // PS/2 bring-up. The default path defers to the platform-bus i8042 driver
    // (`crate::ps2::platform`), which enumerates `PNP0303` via ACPI and claims
    // 0x60/0x64 + IRQ 1 with devres ownership during the probe. The
    // `i8042.legacy` escape hatch runs the hardcoded bring-up here instead.
    if ps2::legacy_mode() {
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
        register_legacy_irq(LEGACY_IRQ_KEYBOARD);
        register_legacy_irq(LEGACY_IRQ_MOUSE);
    }

    cpu::enable_interrupts();
}
