//! Host-side tests for `slopos_ostd::sync::wait_queue`.
//!
//! # Scope (and why most tests are kernel-side)
//!
//! `WaitQueue::wait_event*` and `wake_*` paths take the queue's
//! internal `SpinLock`, which executes the `cli` instruction
//! (privileged in user mode). Under `cargo test` running as a normal
//! Linux process, `cli` raises SIGSEGV. The deep integration tests
//! (mock scheduler backend, concurrent wake/Drop races, panic-mid-wait
//! recovery, full three-way recheck logic) therefore live kernel-side
//! in `core/src/scheduler/sched_tests.rs`, where the per-CPU PCR is
//! initialized and `cli` is permitted.
//!
//! The host-side suite here verifies the parts that touch no
//! privileged state:
//!
//! - `WaitOutcome<R>` API surface — `is_ready`, `into_ready`, derive
//!   correctness.
//! - `WaitQueue::has_waiters()` is **lock-free**; it must read the
//!   queue's intrusive-list head atom without entering the
//!   `SpinLock`. Verifying a fresh queue reports `false` proves the
//!   lock-free read path doesn't crash.
//! - `WaitQueue::generation()` is a plain atomic load.
//! - `WaitOutcome` exhibits `Debug + Clone + Copy + PartialEq + Eq`
//!   as declared (regression guard against accidental derive drift
//!   in future refactors).

use std::sync::Mutex;

use slopos_ostd::sync::wait_queue::{WaitOutcome, WaitQueue};

/// Serializes every test that touches a `WaitQueue`. `cargo test`
/// parallelizes `#[test]` items by default, and even though our
/// host-side suite doesn't toggle the backend `BACKEND_INSTALLED`
/// flag, future tests might. The lock is cheap insurance.
static TEST_LOCK: Mutex<()> = Mutex::new(());

// ----------------------------------------------------------------------------
// WaitOutcome API
// ----------------------------------------------------------------------------

#[test]
fn waitoutcome_is_ready_returns_true_only_for_ready() {
    let _g = TEST_LOCK.lock().unwrap();
    let ready: WaitOutcome<u32> = WaitOutcome::Ready(42);
    let timeout: WaitOutcome<u32> = WaitOutcome::Timeout;
    let no_runtime: WaitOutcome<u32> = WaitOutcome::NoRuntime;
    assert!(ready.is_ready());
    assert!(!timeout.is_ready());
    assert!(!no_runtime.is_ready());
}

#[test]
fn waitoutcome_into_ready_unwraps_ready_only() {
    let _g = TEST_LOCK.lock().unwrap();
    assert_eq!(WaitOutcome::Ready(7).into_ready(), Some(7));
    let timeout: WaitOutcome<u32> = WaitOutcome::Timeout;
    assert_eq!(timeout.into_ready(), None);
    let no_runtime: WaitOutcome<u32> = WaitOutcome::NoRuntime;
    assert_eq!(no_runtime.into_ready(), None);
}

#[test]
fn waitoutcome_carries_non_copy_payload() {
    let _g = TEST_LOCK.lock().unwrap();
    // String is !Copy; WaitOutcome's derive bounds shouldn't require
    // R: Copy. (R: Clone is implied by the derive on the enum.)
    let r: WaitOutcome<String> = WaitOutcome::Ready(String::from("hello"));
    let cloned = r.clone();
    assert!(matches!(cloned, WaitOutcome::Ready(ref s) if s == "hello"));
    drop(r);
}

#[test]
fn waitoutcome_derives_are_present() {
    let _g = TEST_LOCK.lock().unwrap();
    // Force-compile-check that Debug, Clone, Copy, PartialEq, Eq are
    // all derived (Copy implies the trait, etc.). If any derive drops
    // in a future refactor this test breaks at compile time.
    fn assert_debug<T: core::fmt::Debug>() {}
    fn assert_clone<T: Clone>() {}
    fn assert_copy<T: Copy>() {}
    fn assert_partial_eq<T: PartialEq>() {}
    fn assert_eq_<T: Eq>() {}
    assert_debug::<WaitOutcome<u32>>();
    assert_clone::<WaitOutcome<u32>>();
    assert_copy::<WaitOutcome<u32>>();
    assert_partial_eq::<WaitOutcome<u32>>();
    assert_eq_::<WaitOutcome<u32>>();

    // PartialEq cross-variant comparisons.
    let a: WaitOutcome<u32> = WaitOutcome::Ready(1);
    let b: WaitOutcome<u32> = WaitOutcome::Ready(1);
    let c: WaitOutcome<u32> = WaitOutcome::Ready(2);
    let d: WaitOutcome<u32> = WaitOutcome::Timeout;
    let e: WaitOutcome<u32> = WaitOutcome::NoRuntime;
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_ne!(a, d);
    assert_ne!(d, e);
}

// ----------------------------------------------------------------------------
// WaitQueue lock-free observability
// ----------------------------------------------------------------------------

#[test]
fn fresh_queue_has_waiters_is_false_without_taking_the_spinlock() {
    let _g = TEST_LOCK.lock().unwrap();
    // `has_waiters` MUST read the intrusive-list's head atom
    // directly via `SpinLock::as_ptr()` without acquiring the
    // SpinLock — otherwise it would execute `cli` and crash this
    // user-space test. A successful return is therefore proof that
    // the lock-free path is correctly wired.
    let wq = WaitQueue::new();
    assert!(!wq.has_waiters());
}

#[test]
fn fresh_queue_generation_is_zero() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq = WaitQueue::new();
    assert_eq!(wq.generation(), 0);
}

#[test]
fn wait_queue_default_constructor_matches_new() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq1 = WaitQueue::new();
    let wq2 = WaitQueue::default();
    assert_eq!(wq1.has_waiters(), wq2.has_waiters());
    assert_eq!(wq1.generation(), wq2.generation());
}

#[test]
fn multiple_fresh_queues_are_independent() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq1 = WaitQueue::new();
    let wq2 = WaitQueue::new();
    assert!(!wq1.has_waiters());
    assert!(!wq2.has_waiters());
    // generations are independent atomics — both at 0.
    assert_eq!(wq1.generation(), 0);
    assert_eq!(wq2.generation(), 0);
}
