//! Primitive CPU instructions: hlt, pause, halt loop.

use core::arch::asm;

/// Execute the HLT instruction, halting the CPU until the next interrupt.
#[inline(always)]
pub fn hlt() {
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

/// Execute the PAUSE instruction (spin-loop hint).
#[inline(always)]
pub fn pause() {
    unsafe {
        asm!("pause", options(nomem, nostack, preserves_flags));
    }
}

/// Halt forever in a loop. Does not return.
#[inline(always)]
pub fn halt_loop() -> ! {
    loop {
        hlt();
    }
}

/// Atomic `sti; hlt` pair. The IF-shadow rule guarantees no IRQ window
/// between the `sti` and the `hlt`, so a pending interrupt that
/// arrived during a `cli` region is delivered exactly when the CPU is
/// already in HLT state. Used by HPET-driven busy-wait loops in
/// drivers (`drivers/src/virtio/mod.rs::pause_for_irq`).
#[inline(always)]
pub fn sti_hlt_atomic() {
    // SAFETY: `sti; hlt` is a single architectural sequence; the IF
    // shadow keeps interrupts inhibited until the HLT completes its
    // halt transition.
    unsafe {
        asm!("sti", "hlt", options(nomem, nostack));
    }
}
