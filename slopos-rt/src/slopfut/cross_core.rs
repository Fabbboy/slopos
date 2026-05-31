//! `Send` cross-core channel (Phase-6 Tier B, additive).
//!
//! The single-threaded [`sync`](super::sync) channels are `Rc`-backed: their
//! `Sender` and `Receiver` both live on one reactor thread and wake each other
//! through that reactor's ready-queue. They cannot cross threads — an `Rc`
//! waker fired from another core would scribble a foreign reactor's task ids.
//!
//! This channel bridges that gap. The [`Sender`] is `Send + Sync + Clone` and
//! lives on **any** thread; the [`Receiver`] is `!Send` and integrates with one
//! specific reactor. The shared state is an `Arc<Shared<T>>` — a `Mutex`-guarded
//! `VecDeque<T>` plus the write end of that receiver-reactor's wakeup self-pipe
//! ([`reactor::Reactor`]'s lazily-armed `OP_POLL_ADD`).
//!
//! ## Wake path (the load-bearing invariant)
//!
//! A reactor parked in `ring_enter` can only be roused through an fd its ring
//! polls; a `Send` sender on core B cannot touch the `!Send` `Rc` waker of a
//! receiver task on core A. So the cross-thread wake is a self-pipe write, never
//! a direct waker call:
//!
//!   `Sender::send` → lock+push the queue → write one byte to the wakeup-fd
//!     → core A's reactor poll completes in `park` → `service_wakeup` fires the
//!     receiver's **local** waker → the receiver re-polls and drains the queue.
//!
//! The sender only ever touches `Send` state (the mutex + the fd). The `!Send`
//! waker is fired exclusively on the receiver's own thread by its own reactor.
//! This mirrors the windowing `UiSender` self-pipe rationale: a `!Send` runtime
//! is woken cross-thread only via a kernel fd it polls.
//!
//! ## Lost-wakeup safety
//!
//! `send` writes the byte *after* the push, and a receiver that finds the queue
//! empty registers its local waker with the reactor *before* returning Pending
//! — and the reactor's next `park` is what reads the byte and fires that waker.
//! Both the registration and the park run on the receiver's single thread in
//! that order, and the byte (written after the push) is durable in the pipe, so
//! the item is always observed: either the receiver's poll pops it directly, or
//! the byte rouses a re-poll that pops it. A byte left in the pipe after the
//! item was already taken is a harmless spurious wake.
//!
//! ## Deferred (future scope, intentionally not built here)
//!
//! Work-stealing between reactors, per-thread signalfd mask reconciliation, and
//! `futex_wait` timeout enforcement are out of scope: this channel's wake path
//! is the wakeup-fd, so it never needs a timed wait, and the queue mutex (which
//! routes through the kernel futex under contention) needs no condvar.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use slopos_abi::syscall::SYSCALL_FS_WRITE;
use slopos_slibc::pal::raw::syscall3;

use super::reactor;

/// Cross-thread shared channel state. `Send + Sync` (the `Mutex` makes the
/// queue safe to touch from any thread; the fd is a plain integer).
struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    /// Write end of the receiver-reactor's wakeup self-pipe. `-1` if the
    /// receiver's reactor could not arm a wakeup pipe (the channel still works
    /// when sender and receiver happen to share a thread; it just cannot rouse
    /// a parked reactor cross-core).
    wakeup_fd: i32,
}

/// The `Send + Sync + Clone` producer. Lives on any thread; `send` enqueues an
/// item and rouses the receiver's reactor via the wakeup-fd.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// The `!Send` consumer. Bound to the reactor of the thread that created it
/// (the wakeup-fd write end addresses that reactor's self-pipe). Polled as a
/// future via [`Receiver::recv`].
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

/// Create a cross-core channel **on the current reactor**.
///
/// Must be called from within a [`block_on`](super::block_on): it arms (or
/// reuses) the current reactor's wakeup self-pipe so cross-core sends can rouse
/// it. The returned [`Sender`] is `Send` and can be moved to other threads; the
/// [`Receiver`] stays on this thread.
pub fn channel<T: Send>() -> (Sender<T>, Receiver<T>) {
    // Arm this reactor's wakeup pipe and capture its write end. `-1` if the
    // pipe could not be created — the receiver then cannot be roused while
    // parked, but the channel is still sound (and works same-thread).
    let wakeup_fd = reactor::with_reactor(|r| r.ensure_wakeup()).unwrap_or(-1);
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        wakeup_fd,
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

impl<T> Sender<T> {
    /// Enqueue `item` and rouse the receiver's reactor. Lock-and-push, then
    /// write one wakeup byte — in that order, so the item is visible before the
    /// byte that triggers the receiver's re-poll (no lost wakeup).
    pub fn send(&self, item: T) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.push_back(item);
        }
        // Rouse the parked reactor. A full pipe (EAGAIN) means a wakeup is
        // already pending — harmless. Any other error is best-effort.
        if self.shared.wakeup_fd >= 0 {
            let byte = [1u8];
            unsafe {
                syscall3(
                    SYSCALL_FS_WRITE,
                    self.shared.wakeup_fd as u64,
                    byte.as_ptr() as u64,
                    1,
                );
            }
        }
    }
}

// The `Arc<Shared<T>>` is `Send + Sync` whenever `T: Send`, so the sender is
// freely movable across threads — the whole point of the cross-core channel.
unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Sync for Sender<T> {}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Receiver<T> {
    /// Await the next item. Resolves once an item is available; never resolves
    /// to a "closed" state — senders may keep arriving (the consumer decides
    /// when it has received enough). Drains one item per call.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { rx: self }
    }

    /// Non-blocking drain: pop one item if the queue is non-empty.
    pub fn try_recv(&self) -> Option<T> {
        self.shared
            .queue
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
    }
}

/// Future returned by [`Receiver::recv`].
pub struct Recv<'a, T> {
    rx: &'a mut Receiver<T>,
}

impl<T> Future for Recv<'_, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        // Fast path: an item is already queued.
        if let Some(item) = this.rx.try_recv() {
            return Poll::Ready(item);
        }
        // Empty: register this task's local waker with the reactor's wakeup
        // waiter set, then re-check the queue. The reactor (this thread) only
        // services the wakeup-fd inside `park`, which happens strictly after
        // this poll returns Pending — and a sender's byte (written after its
        // push) is durable in the pipe — so the re-check + registration cannot
        // race a concurrent push into a lost wakeup.
        reactor::with_reactor(|r| r.register_wakeup_waiter(cx.waker().clone()));
        if let Some(item) = this.rx.try_recv() {
            return Poll::Ready(item);
        }
        Poll::Pending
    }
}
