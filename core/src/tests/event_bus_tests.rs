//! Typed event-bus primitive tests.
//!
//! The kernel test phase runs as a boot step *before* the scheduler hands
//! over, so there is no current task to enqueue or block — `subscribe_current`
//! and a real `wait`/`wake` round-trip are exercised by the userland test
//! phase (real processes that actually block on poll/read/write). What is
//! observable here, task-free, is the bus's structural correctness:
//!
//!   - every [`KernelEvent`] variant routes to an in-range backing queue
//!     (the `% CAP` index never panics, even for boundary / oversized ids);
//!   - `publish` / `publish_one` on an idle queue wake nobody;
//!   - a [`Subscription`] forwards the condition pre-check to its queue.

use slopos_abi::event::{CHILD_EXIT_BUCKETS, MAX_PIPES, MAX_TTYS, MAX_UNIX_SOCKETS};
use slopos_abi::event::{KernelEvent, PipeSlot, SocketSlot, TaskSlot, TtySlot, UnixSocketSlot};
use slopos_abi::net::MAX_SOCKETS;
use slopos_ostd::sync::BUS;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

/// One representative event per variant, including boundary slot ids and an
/// oversized id that must fold back into range via `% CAP`.
fn sample_events() -> [KernelEvent; 14] {
    [
        KernelEvent::SocketRecv {
            sock: SocketSlot(0),
        },
        KernelEvent::SocketRecv {
            sock: SocketSlot((MAX_SOCKETS - 1) as u32),
        },
        KernelEvent::SocketSend {
            sock: SocketSlot((MAX_SOCKETS + 5) as u32),
        },
        KernelEvent::SocketAccept {
            sock: SocketSlot((MAX_SOCKETS - 1) as u32),
        },
        KernelEvent::PipeRead { pipe: PipeSlot(0) },
        KernelEvent::PipeWrite {
            pipe: PipeSlot((MAX_PIPES - 1) as u32),
        },
        KernelEvent::TtyInput { tty: TtySlot(0) },
        KernelEvent::TtyOutput {
            tty: TtySlot((MAX_TTYS - 1) as u32),
        },
        KernelEvent::UnixSocket {
            sock: UnixSocketSlot(0),
        },
        KernelEvent::UnixSocket {
            sock: UnixSocketSlot((MAX_UNIX_SOCKETS - 1) as u32),
        },
        KernelEvent::ChildExit { task: TaskSlot(0) },
        KernelEvent::ChildExit {
            task: TaskSlot((CHILD_EXIT_BUCKETS - 1) as u32),
        },
        // Oversized ids: must fold into range, not panic.
        KernelEvent::ChildExit {
            task: TaskSlot(u32::MAX),
        },
        KernelEvent::PipeRead {
            pipe: PipeSlot(u32::MAX),
        },
    ]
}

// ---------------------------------------------------------------------------
// Routing — every variant maps to an in-range queue and publishing is safe.
// ---------------------------------------------------------------------------

pub fn test_event_publish_routes_in_range() -> TestResult {
    for ev in sample_events() {
        // No subscriber is registered (no task context), so a publish wakes
        // nobody. The point is that `queue_for` indexes in range for every
        // variant and boundary id without panicking.
        assert_eq_test!(BUS.publish(ev), 0, "idle publish wakes nobody");
        assert_test!(!BUS.publish_one(ev), "idle publish_one wakes nobody");
    }
    pass!()
}

// ---------------------------------------------------------------------------
// Idle bus — fresh queues report no waiters.
// ---------------------------------------------------------------------------

pub fn test_event_idle_queue_has_no_waiters() -> TestResult {
    for ev in sample_events() {
        assert_test!(!BUS.has_waiters(ev), "idle queue has no waiters");
        assert_eq_test!(BUS.waiter_count(ev), 0, "idle queue waiter_count is 0");
    }
    pass!()
}

// ---------------------------------------------------------------------------
// Subscription — forwards the condition pre-check to the backing queue.
// ---------------------------------------------------------------------------

pub fn test_event_subscription_pre_check_paths() -> TestResult {
    let ev = KernelEvent::PipeRead { pipe: PipeSlot(40) };
    // A condition already true returns immediately via the pre-check, without
    // needing a task to block.
    assert_test!(
        BUS.subscribe(ev).wait_event(|| true),
        "wait_event pre-check returns true"
    );
    assert_test!(
        BUS.subscribe(ev).wait_event_timeout(|| true, 100),
        "wait_event_timeout pre-check returns true"
    );
    // An unsatisfiable condition with no task context returns `false`
    // (Timeout or NoRuntime) — never spuriously true — and leaves the queue
    // empty.
    assert_test!(
        !BUS.subscribe(ev).wait_event_timeout(|| false, 1),
        "unsatisfiable timed wait does not report success"
    );
    assert_test!(!BUS.has_waiters(ev), "no waiter left behind");
    pass!()
}

// ---------------------------------------------------------------------------
// stest! registration
// ---------------------------------------------------------------------------

slopos_testing::stest!(name = test_event_publish_routes_in_range, suite = event);
slopos_testing::stest!(name = test_event_idle_queue_has_no_waiters, suite = event);
slopos_testing::stest!(
    name = test_event_subscription_pre_check_paths,
    suite = event
);
