//! Typed event-bus primitive tests.
//!
//! The kernel test phase runs before the scheduler hands over, so there is no
//! current task to enqueue or block: a real `wait`/`wake` round-trip belongs to
//! the userland phase. What is observable here, task-free, is the bus's
//! structural correctness — routing, idle publishes, the subscription
//! pre-check.

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
        KernelEvent::ChildExit {
            task: TaskSlot(u32::MAX),
        },
        KernelEvent::PipeRead {
            pipe: PipeSlot(u32::MAX),
        },
    ]
}

pub fn test_event_publish_routes_in_range() -> TestResult {
    for ev in sample_events() {
        // With no subscriber the publish wakes nobody; the point is that
        // `queue_for` indexes in range for every variant and boundary id.
        assert_eq_test!(BUS.publish(ev), 0, "idle publish wakes nobody");
        assert_test!(!BUS.publish_one(ev), "idle publish_one wakes nobody");
    }
    pass!()
}

pub fn test_event_idle_queue_has_no_waiters() -> TestResult {
    for ev in sample_events() {
        assert_test!(!BUS.has_waiters(ev), "idle queue has no waiters");
        assert_eq_test!(BUS.waiter_count(ev), 0, "idle queue waiter_count is 0");
    }
    pass!()
}

pub fn test_event_subscription_pre_check_paths() -> TestResult {
    let ev = KernelEvent::PipeRead { pipe: PipeSlot(40) };
    assert_test!(
        BUS.subscribe(ev).wait_event(|| true).is_ok(),
        "wait_event pre-check returns true"
    );
    assert_test!(
        BUS.subscribe(ev).wait_event_timeout(|| true, 100).is_ok(),
        "wait_event_timeout pre-check returns true"
    );
    assert_test!(
        BUS.subscribe(ev).wait_event_timeout(|| false, 1).is_err(),
        "unsatisfiable timed wait does not report success"
    );
    assert_test!(!BUS.has_waiters(ev), "no waiter left behind");
    pass!()
}

slopos_testing::stest!(name = test_event_publish_routes_in_range, suite = event);
slopos_testing::stest!(name = test_event_idle_queue_has_no_waiters, suite = event);
slopos_testing::stest!(
    name = test_event_subscription_pre_check_paths,
    suite = event
);

/// AF_INET sockets past `MAX_SOCKETS` get their own queue, not a fold-mate's.
/// A folded index stays correct — every waiter re-checks a predicate over its
/// own socket — but wakes unrelated sockets, so the routing is asserted
/// directly rather than the code shape.
pub fn test_event_socket_queues_do_not_alias() -> TestResult {
    use slopos_abi::net::MAX_SOCKET_SLOTS;

    // The spine is allocated by the socket-create path; do it explicitly so
    // this test does not depend on a socket having been made first.
    assert_test!(
        slopos_ostd::sync::ensure_socket_queues_allocated(),
        "the per-socket wait-queue spine must allocate"
    );

    let recv = |slot: u32| KernelEvent::SocketRecv {
        sock: SocketSlot(slot),
    };

    assert_test!(
        !BUS.shares_queue(recv(0), recv(MAX_SOCKETS as u32)),
        "socket 0 and socket {} still share a queue",
        MAX_SOCKETS
    );
    assert_test!(
        !BUS.shares_queue(recv(1), recv((MAX_SOCKET_SLOTS - 1) as u32)),
        "socket 1 and the last slab slot still share a queue"
    );
    // The positive control: without it the checks above prove nothing about
    // the comparison itself.
    assert_test!(
        BUS.shares_queue(recv(7), recv(7)),
        "one socket must map to one queue"
    );
    assert_test!(
        !BUS.shares_queue(
            recv(3),
            KernelEvent::SocketSend {
                sock: SocketSlot(3)
            }
        ),
        "recv and send for one socket must not share a queue"
    );
    pass!()
}

slopos_testing::stest!(name = test_event_socket_queues_do_not_alias, suite = event);
