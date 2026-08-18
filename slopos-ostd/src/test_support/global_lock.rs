//! Process-wide serialisation for tests that mutate global one-shot
//! state.
//!
//! The lib test binary runs its tests on multiple threads, and several modules
//! reset and re-drive process-global state (the BSP/AP capability mint
//! one-shots, preempt-backend registration, the IRQ allocator bitmap): two
//! interleaving trips the one-shot panic, or lets a count assertion observe a
//! sibling test's live guards. Spin lock because the `test-helpers` feature
//! builds without `std`.

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
/// Any test that resets or re-drives process-global one-shot state must hold
/// this guard for the test's full body.
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
