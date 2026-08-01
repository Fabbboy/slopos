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
//! - `WaitResult<R>` / `WaitAbort` API surface — the payload is carried out
//!   of `Ok`, and every abort is distinguishable.
//! - `WaitQueue::has_waiters()` is **lock-free**; it must read the
//!   queue's intrusive-list head atom without entering the
//!   `SpinLock`. Verifying a fresh queue reports `false` proves the
//!   lock-free read path doesn't crash.
//! - `WaitQueue::generation()` is a plain atomic load.
//! - `WaitAbort` exhibits `Debug + Clone + Copy + PartialEq + Eq`
//!   as declared (regression guard against accidental derive drift
//!   in future refactors).

use std::sync::Mutex;

use slopos_ostd::sync::wait_queue::{WaitAbort, WaitQueue, WaitResult};

/// Serializes every test that touches a `WaitQueue`. `cargo test`
/// parallelizes `#[test]` items by default, and even though our
/// host-side suite doesn't toggle the backend `BACKEND_INSTALLED`
/// flag, future tests might. The lock is cheap insurance.
static TEST_LOCK: Mutex<()> = Mutex::new(());

// ----------------------------------------------------------------------------
// WaitResult / WaitAbort API
// ----------------------------------------------------------------------------

#[test]
fn waitresult_is_ok_only_when_the_condition_held() {
    let _g = TEST_LOCK.lock().unwrap();
    let ready: WaitResult<u32> = Ok(42);
    let timeout: WaitResult<u32> = Err(WaitAbort::Timeout);
    let no_runtime: WaitResult<u32> = Err(WaitAbort::NoRuntime);
    let killed: WaitResult<u32> = Err(WaitAbort::Killed);
    let interrupted: WaitResult<u32> = Err(WaitAbort::Interrupted);
    assert!(ready.is_ok());
    assert!(timeout.is_err());
    assert!(no_runtime.is_err());
    assert!(killed.is_err());
    assert!(interrupted.is_err());
}

#[test]
fn waitresult_carries_its_payload_out() {
    let _g = TEST_LOCK.lock().unwrap();
    assert_eq!(Ok::<u32, WaitAbort>(7).ok(), Some(7));
    let timeout: WaitResult<u32> = Err(WaitAbort::Timeout);
    assert_eq!(timeout.ok(), None);
}

#[test]
fn waitresult_carries_non_copy_payload() {
    let _g = TEST_LOCK.lock().unwrap();
    // String is !Copy; the abort type's derives must not force R: Copy.
    let r: WaitResult<String> = Ok(String::from("hello"));
    let cloned = r.clone();
    assert!(matches!(cloned, Ok(ref s) if s == "hello"));
    drop(r);
}

#[test]
fn waitabort_variants_are_all_distinct() {
    let _g = TEST_LOCK.lock().unwrap();
    // Collapsing any two of these is what the type exists to prevent: a
    // caller must be able to tell "you are dying" from "the deadline passed".
    let all = [
        WaitAbort::Killed,
        WaitAbort::Interrupted,
        WaitAbort::Timeout,
        WaitAbort::NoRuntime,
    ];
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            assert_eq!(i == j, a == b, "{a:?} vs {b:?}");
        }
    }
}

#[test]
fn waitabort_derives_are_present() {
    let _g = TEST_LOCK.lock().unwrap();
    // Force-compile-check that Debug, Clone, Copy, PartialEq, Eq are all
    // derived. If any drops in a future refactor this breaks at compile time.
    fn assert_debug<T: core::fmt::Debug>() {}
    fn assert_clone<T: Clone>() {}
    fn assert_copy<T: Copy>() {}
    fn assert_partial_eq<T: PartialEq>() {}
    fn assert_eq_<T: Eq>() {}
    assert_debug::<WaitAbort>();
    assert_clone::<WaitAbort>();
    assert_copy::<WaitAbort>();
    assert_partial_eq::<WaitAbort>();
    assert_eq_::<WaitAbort>();
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
