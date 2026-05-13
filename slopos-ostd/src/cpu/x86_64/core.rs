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

/// Atomic `sti; hlt; cli` triple — idle-loop park-until-interrupt with
/// IRQs returned to the disabled state on resume. The IF-shadow
/// (same as [`sti_hlt_atomic`]) keeps interrupts inhibited between the
/// `sti` and the `hlt`; the trailing `cli` re-disables IRQs after the
/// woken ISR's IRET restores IF=1, so the caller's surrounding
/// IRQ-disabled discipline is preserved across the wake.
#[inline(always)]
pub fn sti_hlt_cli_atomic() {
    unsafe {
        asm!("sti", "hlt", "cli", options(nomem, nostack));
    }
}

/// Last-resort platform reset: load a zero-limit IDT, then issue
/// `int3`. The breakpoint cannot be vectored through an empty IDT,
/// the resulting double-fault cannot be vectored either, and the CPU
/// triple-faults — most platforms (and QEMU) interpret the triple
/// fault as a reset. Used by the kernel reboot path after the PS/2
/// keyboard-controller reset fails.
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
    // Defensive: if the platform somehow swallows the triple fault,
    // park the BSP rather than letting control escape into surprising
    // territory.
    halt_loop()
}
