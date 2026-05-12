//! Panic-time lock recovery.
//!
//! Inv. 9 (lifetime obligations) relaxes during fatal abort: the kernel
//! is halting, not recovering. We unlock every lock the panicking CPU
//! holds so other CPUs can drain their own halts cleanly, then HLT
//! forever.
//!
//! The per-CPU held-lock stack is maintained by
//! [`lock_tracking::push_lock`] / [`lock_tracking::pop_lock`] which fire
//! from every `SpinLock` / `PreemptMutex` acquire / release. This entry
//! point walks the panicking CPU's stack in reverse, calls each lock's
//! registered poison callback, then halts forever.
//!
//! Today's `boot::panic` handler manually invokes
//! [`lock_tracking::poison_unlock_all_held`] then enters its own halt
//! loop. The wrapper here centralises both halves so consumers stop
//! repeating the `cli; hlt` boilerplate per call site.
//!
//! [`lock_tracking::push_lock`]: super::lock_tracking::push_lock
//! [`lock_tracking::pop_lock`]: super::lock_tracking::pop_lock
//! [`lock_tracking::poison_unlock_all_held`]: super::lock_tracking::poison_unlock_all_held

/// Walk the panicking CPU's held-lock stack, poison-unlock every entry,
/// and halt the CPU forever. Never returns.
///
/// Safe-fn surface: the operation is panic-only by contract, and
/// callers reach this from a `#[panic_handler]` (or hand-off from a
/// fatal NMI watchdog) — both of which already obey the single-writer
/// invariant the held-lock list assumes.
pub fn poison_all_held_locks() -> ! {
    // SAFETY: per-CPU held-lock list is sound to walk from the
    // panicking CPU itself (single-writer). Lock addresses are
    // statics → always valid. Inv. 9 covers the lifetime relaxation
    // during fatal abort.
    unsafe {
        super::lock_tracking::poison_unlock_all_held();
    }
    halt_forever()
}

/// `cli; hlt` loop. Centralised so panic / fatal-exception code paths
/// stop spelling out the asm per site.
#[inline(never)]
fn halt_forever() -> ! {
    loop {
        // SAFETY: HLT with interrupts disabled is the canonical halt.
        // No memory accesses, no stack usage — the `options` reflect
        // that to LLVM so the surrounding panic frame can be elided.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
        // Host-side fallback: `cargo test` may exercise the surface
        // via a `#[should_panic]` test, in which case we still want
        // the function to typecheck as `-> !`. `core::hint::spin_loop`
        // is no-op on host but keeps the loop alive.
        #[cfg(not(target_arch = "x86_64"))]
        {
            core::hint::spin_loop();
        }
    }
}
