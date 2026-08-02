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
    // Fatal: one-way. The diagnostics below this point (SERIAL, the panic
    // screen, the shutdown ritual) acquire locks in an order nothing has
    // validated, on a CPU that may still hold locks from the fault site.
    super::lock_tracking::enter_fatal_bypass();
    // SAFETY: per-CPU held-lock list is sound to walk from the
    // panicking CPU itself (single-writer).
    unsafe {
        super::lock_tracking::poison_unlock_all_held();
    }
    halt_forever()
}

/// Walk the panicking CPU's held-lock stack and poison-unlock every
/// entry, then return control to the caller (no halt).
///
/// Equivalent to [`poison_all_held_locks`] minus the trailing HLT
/// loop — useful for NMI watchdog / fatal-IRET-frame paths that want
/// to free locks held by the dying CPU before chaining into a
/// downstream `panic!()` so other CPUs make progress.
///
/// Safe-fn surface: same contract as [`poison_all_held_locks`].
pub fn poison_all_held_locks_no_halt() {
    // Fatal: one-way. The diagnostics below this point (SERIAL, the panic
    // screen, the shutdown ritual) acquire locks in an order nothing has
    // validated, on a CPU that may still hold locks from the fault site.
    super::lock_tracking::enter_fatal_bypass();
    // SAFETY: as `poison_all_held_locks`.
    unsafe {
        super::lock_tracking::poison_unlock_all_held();
    }
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
