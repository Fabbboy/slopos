//! Process-wide serialisation for tests that mutate global one-shot
//! state.
//!
//! The slopos-ostd lib test binary runs its `#[cfg(test)]` tests on
//! multiple threads, but several test modules reset and re-drive
//! process-global state: the BSP/AP capability mint guards
//! ([`crate::sync::run_bsp_init`]'s one-shot), the preempt-backend
//! registration and its count, the IRQ allocator bitmap, the
//! task-runtime backend flag. Two such tests interleaving corrupt each
//! other's baseline — a reset/mint pair racing another mint trips the
//! one-shot panic, and count assertions observe a sibling test's live
//! guards. Every `isolate()` / `serial()` helper in those modules
//! acquires this lock first, so global-state tests execute serially
//! while the (majority) pure tests keep running in parallel.
//!
//! Plain spin lock on purpose: `no_std` (the `test-helpers` feature
//! builds without `std`), unwind-safe via the RAII guard's `Drop` (a
//! `should_panic` test releases it while unwinding), and hold times
//! are single-test-short, so spinning is cheap.
//!
//! Integration-test binaries are separate processes and serialise
//! themselves (each carries its own `static Mutex<()>`); this lock is
//! about the *lib* binary, where all unit-test modules share one
//! process's globals.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

static LOCKED: AtomicBool = AtomicBool::new(false);

/// RAII witness that the global-test-state lock is held. Releases on
/// drop, including during a `should_panic` unwind. `!Send` so the
/// release happens on the acquiring thread.
pub struct GlobalTestStateGuard {
    _not_send: PhantomData<*mut ()>,
}

/// Acquire the process-wide test-state lock (spins until free).
///
/// Contract: any test that resets or re-drives process-global one-shot
/// state (capability mint guards, registered backends, allocator
/// bitmaps) must hold this guard for the test's full body — typically
/// via the owning module's `isolate()` / `serial()` helper.
pub fn lock_global_test_state() -> GlobalTestStateGuard {
    while LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    GlobalTestStateGuard {
        _not_send: PhantomData,
    }
}

impl Drop for GlobalTestStateGuard {
    fn drop(&mut self) {
        LOCKED.store(false, Ordering::Release);
    }
}
