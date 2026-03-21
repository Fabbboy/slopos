//! Interrupt flag management: sti, cli, irqsave/irqrestore.

use core::arch::asm;

/// Enable interrupts (STI).
#[inline(always)]
pub fn enable_interrupts() {
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

/// Disable interrupts (CLI).
#[inline(always)]
pub fn disable_interrupts() {
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

/// Save RFLAGS and disable interrupts (irqsave pattern).
/// Returns the saved RFLAGS value.
#[inline(always)]
pub fn save_flags_cli() -> u64 {
    let flags: u64;
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) flags,
            options(nomem)
        );
    }
    flags
}

/// Restore interrupt flag from saved RFLAGS (irqrestore pattern).
/// Only re-enables interrupts if they were enabled in the saved flags.
#[inline(always)]
pub fn restore_flags(flags: u64) {
    // Check if IF (bit 9) was set in the saved flags
    if flags & (1 << 9) != 0 {
        enable_interrupts();
    }
}

/// Read RFLAGS register without modifying interrupt state.
/// Use `save_flags_cli()` if you need to disable interrupts atomically.
#[inline(always)]
pub fn read_rflags() -> u64 {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    flags
}

/// Returns true if interrupts are currently enabled (IF bit set).
#[inline(always)]
pub fn are_interrupts_enabled() -> bool {
    (read_rflags() & (1 << 9)) != 0
}
