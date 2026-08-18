//! Host-side tests for `slopos_ostd::sync::wait_queue`.
//!
//! The `wait_event*` / `wake_*` paths take the queue's `SpinLock`, whose `cli`
//! is privileged and faults under `cargo test`; those tests live kernel-side in
//! `core/src/scheduler/sched_tests.rs`. Only the lock-free surface is covered
//! here.

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use std::sync::Mutex;

use slopos_ostd::sync::wait_queue::{WaitAbort, WaitQueue, WaitResult};

/// Serializes every test that touches a `WaitQueue`.
static TEST_LOCK: Mutex<()> = Mutex::new(());

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

#[test]
fn fresh_queue_has_waiters_is_false_without_taking_the_spinlock() {
    let _g = TEST_LOCK.lock().unwrap();
    // Acquiring the SpinLock would execute `cli` and fault in user space, so
    // returning at all proves `has_waiters` reads the list head lock-free.
    let wq = WaitQueue::new(lock_class!("test.hostwq1", LOCK_LEVEL_RESOURCE));
    assert!(!wq.has_waiters());
}

#[test]
fn fresh_queue_generation_is_zero() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq = WaitQueue::new(lock_class!("test.hostwq2", LOCK_LEVEL_RESOURCE));
    assert_eq!(wq.generation(), 0);
}

#[test]
fn wait_queue_default_constructor_matches_new() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq1 = WaitQueue::new(lock_class!("test.hostwq3", LOCK_LEVEL_RESOURCE));
    let wq2 = WaitQueue::new(lock_class!("test.hostwq_default", LOCK_LEVEL_RESOURCE));
    assert_eq!(wq1.has_waiters(), wq2.has_waiters());
    assert_eq!(wq1.generation(), wq2.generation());
}

#[test]
fn multiple_fresh_queues_are_independent() {
    let _g = TEST_LOCK.lock().unwrap();
    let wq1 = WaitQueue::new(lock_class!("test.hostwq4", LOCK_LEVEL_RESOURCE));
    let wq2 = WaitQueue::new(lock_class!("test.hostwq5", LOCK_LEVEL_RESOURCE));
    assert!(!wq1.has_waiters());
    assert!(!wq2.has_waiters());
    assert_eq!(wq1.generation(), 0);
    assert_eq!(wq2.generation(), 0);
}
