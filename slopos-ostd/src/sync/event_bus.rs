//! Typed kernel-wide event bus.
//!
//! One static [`BUS`] owns the wait queues every kernel resource uses to block
//! and wake tasks. Producers call [`EventBus::publish`] with a typed
//! [`KernelEvent`]; blockers call [`EventBus::subscribe`] (single-event
//! blocking) or [`EventBus::subscribe_current`] / [`EventBus::unsubscribe_current`]
//! (register on several events, then block once — the poll/select shape).
//!
//! Routing a [`KernelEvent`] to its backing queue is a single, exhaustively
//! checked match in [`EventBus::queue_for`], so adding a resource variant
//! without wiring its queue is a compile error.
//!
//! A producer must complete every state write that flips the subscribed
//! condition *before* it calls [`publish`](EventBus::publish). The queue's
//! own lock supplies the release/acquire barrier; a consumer woken by the
//! publish therefore observes all of the producer's prior writes.

use crate::lock_class;
use crate::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use crate::sync::once_lock::OnceLock;
use slopos_abi::event::{
    CHILD_EXIT_BUCKETS, KernelEvent, MAX_NETMON, MAX_PIPES, MAX_TTYS, MAX_UNIX_SOCKETS,
    SIGNAL_PENDING_BUCKETS,
};
use slopos_abi::net::{MAX_SOCKET_SLOTS, MAX_SOCKETS};

use crate::KVec;
use crate::sync::wait_queue::{WaitQueue, WaitResult};

/// One AF_INET socket's three wait queues, addressed by slab index.
///
/// Split out so the queues can live on a pinned, boot-allocated spine rather
/// than in the static below; see `SOCKET_QUEUES`.
struct SocketQueues {
    recv: WaitQueue,
    send: WaitQueue,
    accept: WaitQueue,
}

impl SocketQueues {
    fn new() -> Self {
        Self {
            recv: WaitQueue::new(lock_class!("evbus.socket_recv", LOCK_LEVEL_RESOURCE)),
            send: WaitQueue::new(lock_class!("evbus.socket_send", LOCK_LEVEL_RESOURCE)),
            accept: WaitQueue::new(lock_class!("evbus.socket_accept", LOCK_LEVEL_RESOURCE)),
        }
    }
}

/// The AF_INET socket wait queues, one set per slab slot.
///
/// A `WaitQueue` owns an intrusive list of `WaitNode`s whose links are held by
/// parked tasks, so it must never move once a task can be on it. That rules out
/// the socket slab, whose `grow` reallocates its slot vector. The spine is
/// allocated once at its full width and never grows, so `slot` indexes it
/// directly with no fold.
static SOCKET_QUEUES: OnceLock<KVec<SocketQueues>> = OnceLock::new();

/// Allocate the socket queue spine. Idempotent; safe to call from any
/// context that may allocate.
///
/// Returns `false` if the heap refused, in which case [`socket_queues`] falls
/// back to the folded static array below.
pub fn ensure_socket_queues_allocated() -> bool {
    SOCKET_QUEUES.call_once(|| {
        let mut spine = KVec::with_capacity(MAX_SOCKET_SLOTS).unwrap_or_default();
        for _ in 0..MAX_SOCKET_SLOTS {
            if spine.push(SocketQueues::new()).is_err() {
                spine.clear();
                break;
            }
        }
        spine
    });
    SOCKET_QUEUES.get().is_some_and(|spine| !spine.is_empty())
}

/// The kernel's single typed event bus. See the module docs.
pub struct EventBus {
    /// Fallback socket queues, used only before the spine is allocated or if
    /// that allocation was refused. Folded by `% MAX_SOCKETS`.
    socket_recv: [WaitQueue; MAX_SOCKETS],
    socket_send: [WaitQueue; MAX_SOCKETS],
    socket_accept: [WaitQueue; MAX_SOCKETS],
    pipe_read: [WaitQueue; MAX_PIPES],
    pipe_write: [WaitQueue; MAX_PIPES],
    tty_input: [WaitQueue; MAX_TTYS],
    tty_output: [WaitQueue; MAX_TTYS],
    unix_socket: [WaitQueue; MAX_UNIX_SOCKETS],
    child_exit: [WaitQueue; CHILD_EXIT_BUCKETS],
    any_child_exit: [WaitQueue; CHILD_EXIT_BUCKETS],
    signal_pending: [WaitQueue; SIGNAL_PENDING_BUCKETS],
    netmon: [WaitQueue; MAX_NETMON],
}

/// Every queue empty.
///
/// A const rather than a second literal at each site: [`BUS`] and [`TEST_BUS`]
/// are both this value, so the two instances share one lock class per queue
/// array instead of minting a second set of twelve.
const EMPTY_BUS: EventBus = EventBus {
    socket_recv: [const { WaitQueue::new(lock_class!("evbus.socket_recv", LOCK_LEVEL_RESOURCE)) };
        MAX_SOCKETS],
    socket_send: [const { WaitQueue::new(lock_class!("evbus.socket_send", LOCK_LEVEL_RESOURCE)) };
        MAX_SOCKETS],
    socket_accept: [const { WaitQueue::new(lock_class!("evbus.socket_accept", LOCK_LEVEL_RESOURCE)) };
        MAX_SOCKETS],
    pipe_read: [const { WaitQueue::new(lock_class!("evbus.pipe_read", LOCK_LEVEL_RESOURCE)) };
        MAX_PIPES],
    pipe_write: [const { WaitQueue::new(lock_class!("evbus.pipe_write", LOCK_LEVEL_RESOURCE)) };
        MAX_PIPES],
    tty_input: [const { WaitQueue::new(lock_class!("evbus.tty_input", LOCK_LEVEL_RESOURCE)) };
        MAX_TTYS],
    tty_output: [const { WaitQueue::new(lock_class!("evbus.tty_output", LOCK_LEVEL_RESOURCE)) };
        MAX_TTYS],
    unix_socket: [const { WaitQueue::new(lock_class!("evbus.unix_socket", LOCK_LEVEL_RESOURCE)) };
        MAX_UNIX_SOCKETS],
    child_exit: [const { WaitQueue::new(lock_class!("evbus.child_exit", LOCK_LEVEL_RESOURCE)) };
        CHILD_EXIT_BUCKETS],
    any_child_exit: [const { WaitQueue::new(lock_class!("evbus.any_child_exit", LOCK_LEVEL_RESOURCE)) };
        CHILD_EXIT_BUCKETS],
    signal_pending: [const { WaitQueue::new(lock_class!("evbus.signal_pending", LOCK_LEVEL_RESOURCE)) };
        SIGNAL_PENDING_BUCKETS],
    netmon: [const { WaitQueue::new(lock_class!("evbus.netmon", LOCK_LEVEL_RESOURCE)) };
        MAX_NETMON],
};

/// The kernel-wide event bus.
///
/// Const-evaluated, so this large value is laid out as a static and never
/// built on a stack frame.
pub static BUS: EventBus = EMPTY_BUS;

/// A second bus no production path can reach, for the kernel tests that assert
/// on queues with no waiters: proving that accounting property against [`BUS`]
/// both depends on nothing being blocked there and wakes whatever is.
///
/// Socket events route to this instance's own folded arrays rather than the
/// shared [`SOCKET_QUEUES`] spine, which `queue_for` reads from a static: no
/// caller allocates the spine for this bus, and a slot past `MAX_SOCKET_SLOTS`
/// misses it even once some other bus has.
#[cfg(any(test, feature = "test-helpers"))]
pub static TEST_BUS: EventBus = EMPTY_BUS;

impl EventBus {
    /// The pinned queue set for socket slab slot `slot`.
    ///
    /// `None` before the spine is allocated, or if that allocation was
    /// refused, or for a slot outside it — every one of which falls back to
    /// the folded static array.
    ///
    /// A subscriber and a publisher must agree on the queue or a wake is lost,
    /// and they cannot disagree: [`OnceLock::call_once`] stores its result even
    /// when the allocation was refused and produced an empty spine, so there is
    /// exactly one attempt and its outcome is fixed for the life of the boot.
    #[inline]
    fn socket_queues(slot: usize) -> Option<&'static SocketQueues> {
        SOCKET_QUEUES.get()?.get(slot)
    }

    /// Map an event to its backing wait queue.
    ///
    /// The `% CAP` fold is the identity for pipes, TTYs and AF_UNIX, whose ids
    /// are already `< CAP`; task ids are unbounded, so child-exit and
    /// signal-pending really do share buckets. A collision costs a re-check and
    /// nothing else — every waiter parks on a predicate over its *own* state —
    /// and cannot lose a wake, because publish folds by the same rule. AF_INET
    /// sockets index the [`SOCKET_QUEUES`] spine exactly.
    #[inline]
    fn queue_for(&'static self, ev: KernelEvent) -> &'static WaitQueue {
        match ev {
            KernelEvent::SocketRecv { sock } => match Self::socket_queues(sock.0 as usize) {
                Some(queues) => &queues.recv,
                None => &self.socket_recv[(sock.0 as usize) % MAX_SOCKETS],
            },
            KernelEvent::SocketSend { sock } => match Self::socket_queues(sock.0 as usize) {
                Some(queues) => &queues.send,
                None => &self.socket_send[(sock.0 as usize) % MAX_SOCKETS],
            },
            KernelEvent::SocketAccept { sock } => match Self::socket_queues(sock.0 as usize) {
                Some(queues) => &queues.accept,
                None => &self.socket_accept[(sock.0 as usize) % MAX_SOCKETS],
            },
            KernelEvent::PipeRead { pipe } => &self.pipe_read[(pipe.0 as usize) % MAX_PIPES],
            KernelEvent::PipeWrite { pipe } => &self.pipe_write[(pipe.0 as usize) % MAX_PIPES],
            KernelEvent::TtyInput { tty } => &self.tty_input[(tty.0 as usize) % MAX_TTYS],
            KernelEvent::TtyOutput { tty } => &self.tty_output[(tty.0 as usize) % MAX_TTYS],
            KernelEvent::UnixSocket { sock } => {
                &self.unix_socket[(sock.0 as usize) % MAX_UNIX_SOCKETS]
            }
            KernelEvent::ChildExit { task } => {
                &self.child_exit[(task.0 as usize) % CHILD_EXIT_BUCKETS]
            }
            KernelEvent::AnyChildExit { parent } => {
                &self.any_child_exit[(parent.0 as usize) % CHILD_EXIT_BUCKETS]
            }
            KernelEvent::SignalPending { task } => {
                &self.signal_pending[(task.0 as usize) % SIGNAL_PENDING_BUCKETS]
            }
            KernelEvent::NetMonitor { mon } => &self.netmon[(mon.0 as usize) % MAX_NETMON],
        }
    }

    /// Wake every task blocked on `ev`. Returns the number woken.
    #[inline]
    pub fn publish(&'static self, ev: KernelEvent) -> usize {
        self.queue_for(ev).wake_all()
    }

    /// Wake at most one task blocked on `ev`. For single-consumer resources
    /// (e.g. a pipe handing one byte to one of several readers) this preserves
    /// the fairness of a `wake_one` and avoids a thundering herd.
    #[inline]
    pub fn publish_one(&'static self, ev: KernelEvent) -> bool {
        self.queue_for(ev).wake_one()
    }

    /// Enqueue the current task on `ev`'s queue without blocking. Pairs with
    /// [`unsubscribe_current`](EventBus::unsubscribe_current); used by the
    /// poll/select path that registers interest on several events and then
    /// blocks once. Registering before the readiness check is what closes the
    /// lost-wakeup window.
    #[inline]
    pub fn subscribe_current(&'static self, ev: KernelEvent) -> bool {
        self.queue_for(ev).enqueue_current()
    }

    /// Remove the current task from `ev`'s queue. Cleanup partner of
    /// [`subscribe_current`](EventBus::subscribe_current).
    #[inline]
    pub fn unsubscribe_current(&'static self, ev: KernelEvent) {
        self.queue_for(ev).remove_current();
    }

    /// Obtain a typed handle for blocking the current task on a single event.
    #[inline]
    pub fn subscribe(&'static self, ev: KernelEvent) -> Subscription {
        Subscription {
            queue: self.queue_for(ev),
        }
    }

    /// Number of tasks currently blocked on `ev`. Diagnostic / test use.
    #[inline]
    pub fn waiter_count(&'static self, ev: KernelEvent) -> usize {
        self.queue_for(ev).waiter_count()
    }

    /// Whether any task is currently blocked on `ev`. Lock-free.
    #[inline]
    pub fn has_waiters(&'static self, ev: KernelEvent) -> bool {
        self.queue_for(ev).has_waiters()
    }

    /// Whether two events share a backing queue. Diagnostic / test use: the
    /// only way to observe the routing without a task to park.
    #[inline]
    pub fn shares_queue(&'static self, a: KernelEvent, b: KernelEvent) -> bool {
        core::ptr::eq(self.queue_for(a), self.queue_for(b))
    }
}

/// A typed handle for blocking the current task on a single [`KernelEvent`].
///
/// Holds no state of its own — the underlying `wait_event` self-manages its
/// wait node — so dropping it without waiting is a no-op.
#[must_use = "a Subscription does nothing until you wait on it"]
pub struct Subscription {
    queue: &'static WaitQueue,
}

impl Subscription {
    /// Killable block until `condition()` returns `true`.
    #[inline]
    pub fn wait_event<F: FnMut() -> bool>(&self, condition: F) -> WaitResult<()> {
        self.queue.wait_event(condition)
    }

    /// Killable block until `condition()` returns `true` or the deadline
    /// elapses.
    #[inline]
    pub fn wait_event_timeout<F: FnMut() -> bool>(
        &self,
        condition: F,
        timeout_ms: u64,
    ) -> WaitResult<()> {
        self.queue.wait_event_timeout(condition, timeout_ms)
    }

    /// Block until `condition()` returns `true`, aborting on a kill or on any
    /// deliverable signal.
    #[inline]
    pub fn wait_event_interruptible<F: FnMut() -> bool>(&self, condition: F) -> WaitResult<()> {
        self.queue.wait_event_interruptible(condition)
    }

    /// Timed [`wait_event_interruptible`](Self::wait_event_interruptible).
    #[inline]
    pub fn wait_event_interruptible_timeout<F: FnMut() -> bool>(
        &self,
        condition: F,
        timeout_ms: u64,
    ) -> WaitResult<()> {
        self.queue
            .wait_event_interruptible_timeout(condition, timeout_ms)
    }
}
