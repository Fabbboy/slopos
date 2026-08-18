//! Minimal `select` combinators — race N futures, resolve to the first
//! that completes.
//!
//! Each child is polled once per wakeup via `Pin::new(&mut child)`, hence the
//! `Unpin` bound — satisfied by the [`BufOp`](super::BufOp) /
//! [`IntOp`](super::IntOp) leaf futures. The losing children are dropped when
//! the returned `Either` is consumed, which fires their `OP_CANCEL` via
//! [`OpFuture`](super::op)'s `Drop`.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Which of two raced futures completed.
pub enum Either2<A, B> {
    A(A),
    B(B),
}

/// Which of three raced futures completed.
pub enum Either3<A, B, C> {
    A(A),
    B(B),
    C(C),
}

pub struct Select2<A, B> {
    a: Option<A>,
    b: Option<B>,
}

impl<A: Future + Unpin, B: Future + Unpin> Future for Select2<A, B> {
    type Output = Either2<A::Output, B::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(a) = this.a.as_mut() {
            if let Poll::Ready(v) = Pin::new(a).poll(cx) {
                // Drop the loser now: as a match scrutinee this Select can
                // outlive the win, deferring the loser's cancel until then.
                this.b = None;
                return Poll::Ready(Either2::A(v));
            }
        }
        if let Some(b) = this.b.as_mut() {
            if let Poll::Ready(v) = Pin::new(b).poll(cx) {
                this.a = None;
                return Poll::Ready(Either2::B(v));
            }
        }
        Poll::Pending
    }
}

/// Race two futures; resolve to the first that completes. The loser is
/// dropped (and its op cancelled) the moment the winner resolves.
pub fn select2<A: Future + Unpin, B: Future + Unpin>(a: A, b: B) -> Select2<A, B> {
    Select2 {
        a: Some(a),
        b: Some(b),
    }
}

pub struct Select3<A, B, C> {
    a: Option<A>,
    b: Option<B>,
    c: Option<C>,
}

impl<A: Future + Unpin, B: Future + Unpin, C: Future + Unpin> Future for Select3<A, B, C> {
    type Output = Either3<A::Output, B::Output, C::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(a) = this.a.as_mut() {
            if let Poll::Ready(v) = Pin::new(a).poll(cx) {
                this.b = None;
                this.c = None;
                return Poll::Ready(Either3::A(v));
            }
        }
        if let Some(b) = this.b.as_mut() {
            if let Poll::Ready(v) = Pin::new(b).poll(cx) {
                this.a = None;
                this.c = None;
                return Poll::Ready(Either3::B(v));
            }
        }
        if let Some(c) = this.c.as_mut() {
            if let Poll::Ready(v) = Pin::new(c).poll(cx) {
                this.a = None;
                this.b = None;
                return Poll::Ready(Either3::C(v));
            }
        }
        Poll::Pending
    }
}

/// Race three futures; resolve to the first that completes. The losers are
/// dropped (and their ops cancelled) the moment the winner resolves.
pub fn select3<A: Future + Unpin, B: Future + Unpin, C: Future + Unpin>(
    a: A,
    b: B,
    c: C,
) -> Select3<A, B, C> {
    Select3 {
        a: Some(a),
        b: Some(b),
        c: Some(c),
    }
}

/// Run two futures concurrently to completion, resolving to both outputs.
/// Each is `Box::pin`ned (so it need not be `Unpin`) and polled until done.
pub async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let mut ra: Option<A::Output> = None;
    let mut rb: Option<B::Output> = None;
    core::future::poll_fn(move |cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready((ra.take().unwrap(), rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}
