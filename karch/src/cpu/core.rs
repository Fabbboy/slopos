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
