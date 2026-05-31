//! Async synchronization primitives for the single-threaded executor:
//! [`Notify`], a [`oneshot`] channel, and an unbounded [`mpsc`] channel.
//!
//! All are `Rc<RefCell<…>>`-backed (single-threaded — no atomics) and wake
//! waiters through the executor's ready-queue via stored [`Waker`]s.

use core::cell::RefCell;
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
    wakers: VecDeque<Waker>,
}

impl Notify {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify_one(&self) {
        let mut i = self.inner.borrow_mut();
        if let Some(w) = i.wakers.pop_front() {
            w.wake();
        } else {
            i.permit = true;
        }
    }

    pub async fn notified(&self) {
        Notified {
            inner: self.inner.clone(),
        }
        .await
    }
}

struct Notified {
    inner: Rc<RefCell<NotifyInner>>,
}

impl Future for Notified {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut i = self.inner.borrow_mut();
        if i.permit {
            i.permit = false;
            Poll::Ready(())
        } else {
            // Dedup: a `Notified` re-polled before `notify_one` fires (e.g.
            // a sibling branch in a `select`/`join` woke the task) must not
            // enqueue a second copy of the same waker, or a later
            // `notify_one` would spuriously wake an already-resolved task.
            // The waker's data pointer is the task id, so `will_wake` is a
            // cheap identity check (the waiter set is tiny).
            if !i.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                i.wakers.push_back(cx.waker().clone());
            }
            Poll::Pending
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
