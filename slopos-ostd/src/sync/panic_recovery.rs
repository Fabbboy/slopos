//! Panic-time lock recovery.
//!
//! Inv. 9 (lifetime obligations) relaxes during fatal abort: the kernel is
//! halting, not recovering. Every lock the panicking CPU holds is released
//! through its poison callback so other CPUs can drain their own halts.

/// Walk the panicking CPU's held-lock stack, poison-unlock every entry,
/// then halt the CPU forever.
///
/// Safe-fn surface: callers reach this from a `#[panic_handler]` or a
/// fatal NMI watchdog hand-off, both of which already obey the
/// single-writer invariant the held-lock list assumes.
pub fn poison_all_held_locks() -> ! {
    super::lock_tracking::enter_fatal_bypass();
    // SAFETY: per-CPU held-lock list is sound to walk from the
    // panicking CPU itself (single-writer).
    unsafe {
        super::lock_tracking::poison_unlock_all_held();
    }
    halt_forever()
}

/// [`poison_all_held_locks`] minus the trailing halt, for paths that free
/// the dying CPU's locks before chaining into a downstream `panic!()`.
///
/// Safe-fn surface: same contract as [`poison_all_held_locks`].
pub fn poison_all_held_locks_no_halt() {
    super::lock_tracking::enter_fatal_bypass();
    // SAFETY: as `poison_all_held_locks`.
    unsafe {
        super::lock_tracking::poison_unlock_all_held();
    }
}

#[inline(never)]
fn halt_forever() -> ! {
    loop {
        // SAFETY: HLT with interrupts disabled is the canonical halt; no
        // memory accesses and no stack usage, as `options` asserts.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
        // Host builds still need the loop to typecheck as `-> !`.
        #[cfg(not(target_arch = "x86_64"))]
        {
            core::hint::spin_loop();
        }
    }
}
