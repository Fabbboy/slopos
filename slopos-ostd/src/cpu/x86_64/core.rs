//! Primitive CPU instructions: hlt, pause, halt loop.

use core::arch::asm;

#[inline(always)]
pub fn hlt() {
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub fn pause() {
    unsafe {
        asm!("pause", options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub fn halt_loop() -> ! {
    loop {
        hlt();
    }
}

/// Atomic `sti; hlt`: the IF shadow leaves no IRQ window between the two, so
/// an interrupt pending from a `cli` region is delivered with the CPU already
/// in HLT state.
#[inline(always)]
pub fn sti_hlt_atomic() {
    // SAFETY: `sti; hlt` is a single architectural sequence; the IF
    // shadow keeps interrupts inhibited until the HLT completes its
    // halt transition.
    unsafe {
        asm!("sti", "hlt", options(nomem, nostack));
    }
}

/// Atomic `sti; hlt; cli`: park until interrupt as [`sti_hlt_atomic`], then
/// re-disable IRQs after the woken ISR's IRET restores IF=1, preserving the
/// caller's IRQ-disabled discipline across the wake.
#[inline(always)]
pub fn sti_hlt_cli_atomic() {
    unsafe {
        asm!("sti", "hlt", "cli", options(nomem, nostack));
    }
}

/// Last-resort platform reset: a zero-limit IDT can vector neither the `int3`
/// nor the resulting double fault, so the CPU triple-faults, which QEMU and
/// most platforms treat as a reset.
#[inline(always)]
pub fn trigger_triple_fault() -> ! {
    #[repr(C, packed)]
    struct InvalidIdt {
        limit: u16,
        base: u64,
    }
    let invalid = InvalidIdt { limit: 0, base: 0 };
    unsafe {
        asm!(
            "lidt [{0}]",
            "int3",
            in(reg) &invalid,
            options(nostack, preserves_flags),
        );
    }
    // Unreachable unless the platform swallows the triple fault.
    halt_loop()
}
