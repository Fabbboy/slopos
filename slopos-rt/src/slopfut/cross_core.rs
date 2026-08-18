//! `Send` cross-core channel. The [`Sender`] is `Send + Sync + Clone` and lives
//! on **any** thread; the [`Receiver`] is `!Send` and belongs to one reactor.
//! The shared state is an `Arc<Shared<T>>` — a `Mutex`-guarded `VecDeque<T>`
//! plus the write end of that receiver-reactor's wakeup self-pipe.
//!
//! The cross-thread wake is a self-pipe write, never a direct waker call: a
//! reactor parked in `ring_enter` is rousable only through an fd its ring polls,
//! and a sender on another core cannot touch the receiver's `!Send` `Rc` waker.
//! `send` pushes and writes one byte; the receiver's own reactor reads it in
//! `park` and fires the local waker, so the sender only ever touches `Send`
//! state.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use slopos_abi::syscall::SYSCALL_FS_WRITE;
use slopos_slibc::pal::raw::syscall3;

use super::reactor;

/// Cross-thread shared channel state.
struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    /// Write end of the receiver-reactor's wakeup self-pipe; `-1` if no pipe
    /// could be armed, in which case a parked reactor cannot be roused
    /// cross-core (the channel still works same-thread).
    wakeup_fd: i32,
}

/// The `Send + Sync + Clone` producer. Lives on any thread; `send` enqueues an
/// item and rouses the receiver's reactor via the wakeup-fd.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// The `!Send` consumer, bound to the reactor of the thread that created it —
/// the wakeup-fd write end addresses that reactor's self-pipe.
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
    /// Enqueue `item` and rouse the receiver's reactor. Push, then write one
    /// wakeup byte — in that order, so the item is visible before the byte that
    /// triggers the receiver's re-poll.
    pub fn send(&self, item: T) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.push_back(item);
        }
        // The write result is ignored: a full pipe (EAGAIN) means a wakeup is
        // already pending.
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

// `Arc<Shared<T>>` is `Send + Sync` whenever `T: Send`.
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
    /// Await the next item. There is no "closed" state — senders may keep
    /// arriving, so the consumer decides when it has received enough.
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
        if let Some(item) = this.rx.try_recv() {
            return Poll::Ready(item);
        }
        // Register before the re-check: this thread's reactor services the
        // wakeup-fd only inside `park`, which runs strictly after this poll
        // returns Pending, and the sender's byte (written after its push) is
        // durable in the pipe — so a concurrent push cannot be missed.
        reactor::with_reactor(|r| r.register_wakeup_waiter(cx.waker().clone()));
        if let Some(item) = this.rx.try_recv() {
            return Poll::Ready(item);
        }
        Poll::Pending
    }
}
