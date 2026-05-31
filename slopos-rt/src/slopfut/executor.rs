//! The single-threaded task scheduler.
//!
//! Real wakers (no re-poll-everything): a task is polled only when its waker
//! fires (a ring completion via the reactor, or another task via a channel /
//! `Notify`). [`block_on`] drives a root future to completion while running
//! any [`spawn`]ed tasks concurrently on the same thread; the reactor's
//! `park` (a blocking `ring_enter`) is the only sleep point (SLOPRING §7.1).
//!
//! Two thread-locals — this scheduler and the [`reactor`](super::reactor) —
//! are deliberately separate cells so the reactor's `park` can fire a waker
//! (which borrows *this* cell) without a same-cell re-borrow.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use super::reactor;
use super::waker::waker_for;
use crate::ring::Ring;

/// Waker id reserved for the `block_on` root future (which lives as a local,
/// not in the task table). Spawned tasks get ids from 0 upward.
const ROOT_ID: u64 = u64::MAX;

type BoxFuture = Pin<Box<dyn Future<Output = ()>>>;

struct Sched {
    tasks: HashMap<u64, BoxFuture>,
    ready: VecDeque<u64>,
    next_id: u64,
    root_ready: bool,
}

thread_local! {
    static SCHED: RefCell<Option<Sched>> = const { RefCell::new(None) };
}

fn with_sched<R>(f: impl FnOnce(&mut Sched) -> R) -> R {
    SCHED.with(|c| {
        let mut b = c.borrow_mut();
        let s = b
            .as_mut()
            .expect("slopfut: no executor installed — use block_on");
        f(s)
    })
}

/// Schedule task `id` for (re-)poll. Called from the [`waker`](super::waker).
pub(super) fn wake_task(id: u64) {
    SCHED.with(|c| {
        if let Some(s) = c.borrow_mut().as_mut() {
            if id == ROOT_ID {
                s.root_ready = true;
            } else if !s.ready.contains(&id) {
                s.ready.push_back(id);
            }
        }
    });
}

/// Spawn a concurrent task on the current executor. The returned
/// [`JoinHandle`] resolves to the task's output when it completes.
///
/// Must be called from within a [`block_on`] (panics otherwise). The future
/// is `'static` because it outlives the spawning call.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let state: Rc<RefCell<JoinState<F::Output>>> = Rc::new(RefCell::new(JoinState {
        result: None,
        waker: None,
    }));
    let state2 = state.clone();
    let wrapped = async move {
        let out = fut.await;
        let mut st = state2.borrow_mut();
        st.result = Some(out);
        if let Some(w) = st.waker.take() {
            w.wake();
        }
    };
    with_sched(|s| {
        let id = s.next_id;
        s.next_id += 1;
        s.tasks.insert(id, Box::pin(wrapped));
        s.ready.push_back(id);
    });
    JoinHandle { state }
}

struct JoinState<T> {
    result: Option<T>,
    waker: Option<core::task::Waker>,
}

/// Awaitable handle to a [`spawn`]ed task's result.
pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut st = self.state.borrow_mut();
        match st.result.take() {
            Some(v) => Poll::Ready(v),
            None => {
                st.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Poll every currently-ready spawned task once. A task that returns
/// `Pending` is retained; one that returns `Ready` is dropped (its
/// `JoinHandle` already captured the output). Polling may enqueue more tasks
/// (e.g. a channel send wakes a receiver); the loop drains them too.
fn run_ready_tasks() {
    loop {
        let Some(id) = with_sched(|s| s.ready.pop_front()) else {
            break;
        };
        // Take the task out so its poll does not hold the scheduler borrow
        // (the future may submit ring ops or wake other tasks).
        let Some(mut fut) = with_sched(|s| s.tasks.remove(&id)) else {
            continue; // already finished / cancelled
        };
        let waker = waker_for(id);
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(()) => { /* drop the task */ }
            Poll::Pending => with_sched(|s| {
                s.tasks.insert(id, fut);
            }),
        }
    }
}

/// Drive `fut` to completion on `ring`, running spawned tasks concurrently.
/// Consumes the ring (unmapped + closed on return). Not re-entrant.
pub fn block_on<F: Future>(ring: Ring, fut: F) -> F::Output {
    reactor::install(ring);
    SCHED.with(|c| {
        let mut b = c.borrow_mut();
        assert!(b.is_none(), "slopfut: block_on is not re-entrant");
        *b = Some(Sched {
            tasks: HashMap::new(),
            ready: VecDeque::new(),
            next_id: 0,
            root_ready: false,
        });
    });

    let mut root = Box::pin(fut);
    let root_waker = waker_for(ROOT_ID);
    let mut cx = Context::from_waker(&root_waker);
    let mut poll_root = true;

    let output = loop {
        if poll_root {
            poll_root = false;
            if let Poll::Ready(v) = root.as_mut().poll(&mut cx) {
                break v;
            }
        }
        // Run any ready spawned tasks (may wake the root / each other).
        run_ready_tasks();
        if take_root_ready() {
            poll_root = true;
            continue;
        }
        // Quiescent: re-poll only happens via a wakeup. If ops are in flight,
        // park on the ring (the sole sleep) to drive their completions;
        // otherwise nothing can make progress — a genuine deadlock.
        if reactor::with_reactor(|r| r.in_flight()) > 0 {
            reactor::with_reactor(|r| r.park());
            if take_root_ready() {
                poll_root = true;
            }
        } else if with_sched(|s| s.ready.is_empty()) {
            panic!("slopfut: stalled — root Pending with no in-flight op and no ready task");
        }
    };

    // Drop the root (cancels any op it still holds) and any lingering spawned
    // tasks while the reactor is still installed, then tear it down — which
    // closes the ring fd before freeing orphaned buffers.
    drop(root);
    SCHED.with(|c| {
        *c.borrow_mut() = None;
    });
    reactor::uninstall();
    output
}

fn take_root_ready() -> bool {
    with_sched(|s| core::mem::take(&mut s.root_ready))
}

/// Yield control to the executor once: re-enqueue this task and let other
/// ready tasks run before it is polled again. Cooperative scheduling for
/// CPU-bound async work, so one task cannot starve its siblings on the
/// single executor thread.
pub async fn yield_now() {
    let mut yielded = false;
    core::future::poll_fn(move |cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
