//! Typed event-bus primitive tests.
//!
//! The kernel test phase runs before the scheduler hands over, so there is no
//! current task to block: only task-free structure is observable here, and a
//! real `wait`/`wake` round-trip belongs to the userland phase.

use slopos_abi::event::{CHILD_EXIT_BUCKETS, MAX_PIPES, MAX_TTYS, MAX_UNIX_SOCKETS};
use slopos_abi::event::{KernelEvent, PipeSlot, SocketSlot, TaskSlot, TtySlot, UnixSocketSlot};
use slopos_abi::net::{MAX_SOCKET_SLOTS, MAX_SOCKETS};
use slopos_ostd::sync::{BUS, TEST_BUS};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

/// One representative event per variant, including boundary slot ids and an
/// oversized id that must fold back into range via `% CAP`.
/// Below `MAX_SOCKET_SLOTS` `queue_for` routes to the spine, which every bus shares.
fn sample_events() -> [KernelEvent; 14] {
    [
        KernelEvent::SocketRecv {
            sock: SocketSlot(MAX_SOCKET_SLOTS as u32),
        },
        KernelEvent::SocketRecv {
            sock: SocketSlot((MAX_SOCKET_SLOTS + MAX_SOCKETS - 1) as u32),
        },
        KernelEvent::SocketSend {
            sock: SocketSlot((MAX_SOCKET_SLOTS + 5) as u32),
        },
        KernelEvent::SocketAccept {
            sock: SocketSlot(u32::MAX),
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

/// An unbounded id paired with the in-range id it must fold onto.
fn folded_pairs() -> [(KernelEvent, KernelEvent); 6] {
    const HUGE: usize = u32::MAX as usize;
    [
        (
            KernelEvent::PipeRead {
                pipe: PipeSlot(u32::MAX),
            },
            KernelEvent::PipeRead {
                pipe: PipeSlot((HUGE % MAX_PIPES) as u32),
            },
        ),
        (
            KernelEvent::PipeWrite {
                pipe: PipeSlot(u32::MAX),
            },
            KernelEvent::PipeWrite {
                pipe: PipeSlot((HUGE % MAX_PIPES) as u32),
            },
        ),
        (
            KernelEvent::TtyInput {
                tty: TtySlot(u32::MAX),
            },
            KernelEvent::TtyInput {
                tty: TtySlot((HUGE % MAX_TTYS) as u32),
            },
        ),
        (
            KernelEvent::TtyOutput {
                tty: TtySlot(u32::MAX),
            },
            KernelEvent::TtyOutput {
                tty: TtySlot((HUGE % MAX_TTYS) as u32),
            },
        ),
        (
            KernelEvent::UnixSocket {
                sock: UnixSocketSlot(u32::MAX),
            },
            KernelEvent::UnixSocket {
                sock: UnixSocketSlot((HUGE % MAX_UNIX_SOCKETS) as u32),
            },
        ),
        (
            KernelEvent::ChildExit {
                task: TaskSlot(u32::MAX),
            },
            KernelEvent::ChildExit {
                task: TaskSlot((HUGE % CHILD_EXIT_BUCKETS) as u32),
            },
        ),
    ]
}

pub fn test_event_publish_routes_in_range() -> TestResult {
    for ev in sample_events() {
        assert_eq_test!(TEST_BUS.publish(ev), 0, "idle publish wakes nobody");
        assert_test!(!TEST_BUS.publish_one(ev), "idle publish_one wakes nobody");
    }
    for (oversized, folded) in folded_pairs() {
        assert_test!(
            BUS.shares_queue(oversized, folded),
            "an out-of-range slot id does not share the queue it folds onto — \
             a publish and a subscribe that disagree lose the wake"
        );
    }
    pass!()
}

pub fn test_event_idle_queue_has_no_waiters() -> TestResult {
    for ev in sample_events() {
        assert_test!(!TEST_BUS.has_waiters(ev), "idle queue has no waiters");
        assert_eq_test!(TEST_BUS.waiter_count(ev), 0, "idle queue waiter_count is 0");
    }
    pass!()
}

pub fn test_event_subscription_pre_check_paths() -> TestResult {
    let ev = KernelEvent::PipeRead { pipe: PipeSlot(40) };
    assert_test!(
        TEST_BUS.subscribe(ev).wait_event(|| true).is_ok(),
        "wait_event pre-check returns true"
    );
    assert_test!(
        TEST_BUS
            .subscribe(ev)
            .wait_event_timeout(|| true, 100)
            .is_ok(),
        "wait_event_timeout pre-check returns true"
    );
    assert_test!(
        TEST_BUS
            .subscribe(ev)
            .wait_event_timeout(|| false, 1)
            .is_err(),
        "unsatisfiable timed wait does not report success"
    );
    assert_test!(!TEST_BUS.has_waiters(ev), "no waiter left behind");
    pass!()
}

slopos_testing::stest!(name = test_event_publish_routes_in_range, suite = event);
slopos_testing::stest!(name = test_event_idle_queue_has_no_waiters, suite = event);
slopos_testing::stest!(
    name = test_event_subscription_pre_check_paths,
    suite = event
);

/// AF_INET sockets past `MAX_SOCKETS` get their own queue, not a fold-mate's.
/// A folded index would still be correct — waiters re-check their predicate —
/// but would wake unrelated sockets.
pub fn test_event_socket_queues_do_not_alias() -> TestResult {
    // Normally allocated by the socket-create path; done explicitly so this
    // test does not depend on a socket having been made first.
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
    // Positive control: without it the checks above prove nothing about
    // `shares_queue` itself.
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
