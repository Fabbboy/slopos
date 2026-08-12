//! Async synchronization primitives for the single-threaded executor:
//! [`Notify`], a [`oneshot`] channel, and an unbounded [`mpsc`] channel.
//!
//! All are `Rc<RefCell<…>>`-backed (single-threaded — no atomics) and wake
//! waiters through the executor's ready-queue via stored [`Waker`]s.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::rc::Rc;

// ── Notify ─────────────────────────────────────────────────────────────────

/// A multi-waiter notification with a single stored permit (the shape of
/// `tokio::sync::Notify`). `notify_one` wakes one waiter, or leaves a permit
/// for the next `notified().await`.
#[derive(Clone, Default)]
pub struct Notify {
    inner: Rc<RefCell<NotifyInner>>,
}

#[derive(Default)]
struct NotifyInner {
    permit: bool,
    waiters: VecDeque<Waiter>,
}

/// A parked `Notified`, and the flag that tells it — once it is polled again —
/// that the wake it just received was its own.
///
/// The flag is what makes the handoff survive the round trip through the
/// executor. Waking a waker only schedules a re-poll; without a record that
/// *this* future was the one chosen, the re-poll finds no permit and parks
/// again, and the notification is lost for good.
struct Waiter {
    notified: Rc<Cell<bool>>,
    waker: Waker,
}

impl Notify {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify_one(&self) {
        let mut i = self.inner.borrow_mut();
        match i.waiters.pop_front() {
            Some(w) => {
                w.notified.set(true);
                w.waker.wake();
            }
            None => i.permit = true,
        }
    }

    pub fn notified(&self) -> Notified {
        Notified {
            inner: self.inner.clone(),
            state: Rc::new(Cell::new(false)),
            registered: false,
        }
    }
}

pub struct Notified {
    inner: Rc<RefCell<NotifyInner>>,
    /// Set by `notify_one` when this future is the one it woke.
    state: Rc<Cell<bool>>,
    registered: bool,
}

impl Future for Notified {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.state.replace(false) {
            return Poll::Ready(());
        }
        let mut i = this.inner.borrow_mut();
        if i.permit {
            i.permit = false;
            return Poll::Ready(());
        }
        // Register once per future, not once per poll: a re-poll from a
        // sibling branch of a `select`/`join` must not enqueue a second
        // waiter, or one `notify_one` would be consumed without resolving
        // anything.
        if this.registered {
            if let Some(w) = i
                .waiters
                .iter_mut()
                .find(|w| Rc::ptr_eq(&w.notified, &this.state))
            {
                w.waker.clone_from(cx.waker());
            }
        } else {
            i.waiters.push_back(Waiter {
                notified: this.state.clone(),
                waker: cx.waker().clone(),
            });
            this.registered = true;
        }
        Poll::Pending
    }
}

impl Drop for Notified {
    fn drop(&mut self) {
        let mut i = self.inner.borrow_mut();
        i.waiters.retain(|w| !Rc::ptr_eq(&w.notified, &self.state));
        // Dropped after being chosen but before observing it: hand the
        // notification on rather than swallowing it, or a `select` that
        // cancels this branch silently eats another waiter's wakeup.
        if self.state.get() {
            match i.waiters.pop_front() {
                Some(w) => {
                    w.notified.set(true);
                    w.waker.wake();
                }
                None => i.permit = true,
            }
        }
    }
}

// ── oneshot ────────────────────────────────────────────────────────────────

struct OneshotInner<T> {
    value: Option<T>,
    waker: Option<Waker>,
    sender_dropped: bool,
}

/// Create a one-shot channel. The receiver resolves to `Some(value)` once
/// sent, or `None` if the sender is dropped without sending.
pub fn oneshot<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let inner = Rc::new(RefCell::new(OneshotInner {
        value: None,
        waker: None,
        sender_dropped: false,
    }));
    (
        OneshotSender {
            inner: inner.clone(),
        },
        OneshotReceiver { inner },
    )
}

pub struct OneshotSender<T> {
    inner: Rc<RefCell<OneshotInner<T>>>,
}

impl<T> OneshotSender<T> {
    pub fn send(self, value: T) {
        let mut i = self.inner.borrow_mut();
        i.value = Some(value);
        if let Some(w) = i.waker.take() {
            w.wake();
        }
    }
}

impl<T> Drop for OneshotSender<T> {
    fn drop(&mut self) {
        let mut i = self.inner.borrow_mut();
        i.sender_dropped = true;
        if let Some(w) = i.waker.take() {
            w.wake();
        }
    }
}

pub struct OneshotReceiver<T> {
    inner: Rc<RefCell<OneshotInner<T>>>,
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Option<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut i = self.inner.borrow_mut();
        if let Some(v) = i.value.take() {
            Poll::Ready(Some(v))
        } else if i.sender_dropped {
            Poll::Ready(None)
        } else {
            i.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ── unbounded mpsc ───────────────────────────────────────────────────────────

struct ChanInner<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
}

/// Create an unbounded multi-producer / single-consumer channel.
pub fn unbounded<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let inner = Rc::new(RefCell::new(ChanInner {
        queue: VecDeque::new(),
        waker: None,
        senders: 1,
    }));
    (
        UnboundedSender {
            inner: inner.clone(),
        },
        UnboundedReceiver { inner },
    )
}

pub struct UnboundedSender<T> {
    inner: Rc<RefCell<ChanInner<T>>>,
}

impl<T> UnboundedSender<T> {
    pub fn send(&self, value: T) {
        let mut i = self.inner.borrow_mut();
        i.queue.push_back(value);
        if let Some(w) = i.waker.take() {
            w.wake();
        }
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().senders += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        let mut i = self.inner.borrow_mut();
        i.senders -= 1;
        if i.senders == 0 {
            if let Some(w) = i.waker.take() {
                w.wake();
            }
        }
    }
}

pub struct UnboundedReceiver<T> {
    inner: Rc<RefCell<ChanInner<T>>>,
}

impl<T> UnboundedReceiver<T> {
    /// Receive the next value, or `None` once all senders are dropped and the
    /// queue is drained.
    pub async fn recv(&mut self) -> Option<T> {
        Recv { inner: &self.inner }.await
    }
}

struct Recv<'a, T> {
    inner: &'a Rc<RefCell<ChanInner<T>>>,
}

impl<T> Future for Recv<'_, T> {
    type Output = Option<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut i = self.inner.borrow_mut();
        if let Some(v) = i.queue.pop_front() {
            Poll::Ready(Some(v))
        } else if i.senders == 0 {
            Poll::Ready(None)
        } else {
            i.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
