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
//! # Atomic-publish contract
//!
//! A producer must complete every state write that flips the subscribed
//! condition *before* it calls [`publish`](EventBus::publish). The queue's
//! own lock supplies the release/acquire barrier; a consumer woken by the
//! publish therefore observes all of the producer's prior writes. This is the
//! same contract a bare `WaitQueue::wake_all` carries — the bus only changes
//! how the queue is named, not the ordering rules.

use crate::lock_class;
use crate::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_abi::event::{
    CHILD_EXIT_BUCKETS, KernelEvent, MAX_NETMON, MAX_PIPES, MAX_TTYS, MAX_UNIX_SOCKETS,
    SIGNAL_PENDING_BUCKETS,
};
use slopos_abi::net::MAX_SOCKETS;

use crate::sync::wait_queue::{WaitQueue, WaitResult};

/// The kernel's single typed event bus. See the module docs.
pub struct EventBus {
    socket_recv: [WaitQueue; MAX_SOCKETS],
    socket_send: [WaitQueue; MAX_SOCKETS],
    socket_accept: [WaitQueue; MAX_SOCKETS],
    pipe_read: [WaitQueue; MAX_PIPES],
    pipe_write: [WaitQueue; MAX_PIPES],
    tty_input: [WaitQueue; MAX_TTYS],
    tty_output: [WaitQueue; MAX_TTYS],
    unix_socket: [WaitQueue; MAX_UNIX_SOCKETS],
    child_exit: [WaitQueue; CHILD_EXIT_BUCKETS],
    signal_pending: [WaitQueue; SIGNAL_PENDING_BUCKETS],
    netmon: [WaitQueue; MAX_NETMON],
}

/// The kernel-wide event bus.
///
/// Initialised with a direct struct literal so there is no runtime
/// constructor for this large value — the backing queues are laid out as
/// zero-initialised statics, never built on a stack frame.
pub static BUS: EventBus = EventBus {
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
    signal_pending: [const { WaitQueue::new(lock_class!("evbus.signal_pending", LOCK_LEVEL_RESOURCE)) };
        SIGNAL_PENDING_BUCKETS],
    netmon: [const { WaitQueue::new(lock_class!("evbus.netmon", LOCK_LEVEL_RESOURCE)) };
        MAX_NETMON],
};

impl EventBus {
    /// Map an event to its backing wait queue. The `% CAP` keeps the index in
    /// range; for socket/pipe/tty/unix slots the id is already `< CAP`, so the
    /// modulo is the identity. For child-exit, task ids exceed the bucket
    /// count and several tasks share a bucket (collisions are benign — the
    /// waiter re-checks its own exit condition).
    #[inline]
    fn queue_for(&'static self, ev: KernelEvent) -> &'static WaitQueue {
        match ev {
            KernelEvent::SocketRecv { sock } => &self.socket_recv[(sock.0 as usize) % MAX_SOCKETS],
            KernelEvent::SocketSend { sock } => &self.socket_send[(sock.0 as usize) % MAX_SOCKETS],
            KernelEvent::SocketAccept { sock } => {
                &self.socket_accept[(sock.0 as usize) % MAX_SOCKETS]
            }
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
    /// lost-wakeup window (a producer that publishes between the enqueue and
    /// the check has already marked this task).
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
}

/// A typed handle for blocking the current task on a single [`KernelEvent`].
///
/// Replaces the bare `wait_queue.wait_event(closure)` call shape: the closure
/// re-checks the resource condition under its own lock, exactly as before —
/// only the queue lookup is now typed and centralised. The handle holds no
/// state of its own (the underlying `wait_event` self-manages its wait node),
/// so dropping it without waiting is a no-op.
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
