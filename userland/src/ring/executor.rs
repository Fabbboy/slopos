//! `slopfut` — a minimal SlopRing-driven async runtime.
//!
//! This is the smallest credible executor that turns ring completions
//! into resolved futures. It is **userland**, so `#![forbid(unsafe_code)]`
//! does not apply (userland is outside the kernel discipline). It is
//! deliberately single-threaded and hand-rolled rather than vendoring a
//! full embassy-style reactor: SlopOS's goal here is to *demonstrate the
//! edge*, not to ship a production async stack.
//!
//! Model: a caller submits an op tagged with a `user_data` cookie and
//! gets back a [`CompletionFuture`]. Polling the future drains the ring's
//! CQ (via a blocking `ring_enter`, so deferred completions progress —
//! SLOPRING § 7.1) and resolves when the matching cookie arrives.
//! Cancellation is explicit: [`RingExecutor::cancel`] submits an
//! `OP_CANCEL` for an unresolved future's op. (A `CompletionFuture` holds
//! no handle to its executor, so it cannot self-cancel on drop — async
//! cancellation is a deliberate call, not a destructor side effect.)

use std::collections::HashMap;

use slopos_abi::ring::{Cqe, OP_CANCEL, Sqe};

use super::{Ring, RingError};

/// A pending ring operation. Resolves to the CQE `res` once its
/// `user_data` completion lands.
pub struct CompletionFuture {
    user_data: u64,
    fd: i32,
    resolved: Option<i32>,
}

impl CompletionFuture {
    fn new(user_data: u64, fd: i32) -> Self {
        Self {
            user_data,
            fd,
            resolved: None,
        }
    }

    /// The correlation cookie this future waits on.
    pub fn user_data(&self) -> u64 {
        self.user_data
    }

    /// `true` once the completion has landed.
    pub fn is_ready(&self) -> bool {
        self.resolved.is_some()
    }

    /// The resolved result, if ready.
    pub fn result(&self) -> Option<i32> {
        self.resolved
    }
}

/// A single-threaded executor over one [`Ring`].
pub struct RingExecutor {
    ring: Ring,
    /// Completions harvested but not yet matched to a future.
    pending: HashMap<u64, i32>,
    next_cookie: u64,
}

impl RingExecutor {
    /// Wrap a ring in an executor.
    pub fn new(ring: Ring) -> Self {
        Self {
            ring,
            pending: HashMap::new(),
            next_cookie: 1,
        }
    }

    /// Borrow the underlying ring (for direct submission).
    pub fn ring_mut(&mut self) -> &mut Ring {
        &mut self.ring
    }

    /// Allocate a fresh correlation cookie.
    pub fn alloc_cookie(&mut self) -> u64 {
        let c = self.next_cookie;
        self.next_cookie = self.next_cookie.wrapping_add(1);
        c
    }

    /// Submit `sqe` (its `user_data` is overwritten with a fresh cookie)
    /// and return a future tracking its completion. The submission is
    /// published immediately via `ring_enter` (no batching here — the
    /// demonstration favours clarity).
    pub fn submit(&mut self, mut sqe: Sqe) -> Result<CompletionFuture, RingError> {
        let cookie = self.alloc_cookie();
        sqe.user_data = cookie;
        let fd = sqe.fd;
        self.ring.push_sqe(&sqe)?;
        self.ring.submit()?;
        Ok(CompletionFuture::new(cookie, fd))
    }

    /// Drain all currently-available CQEs into the pending map.
    fn drain_cq(&mut self) {
        while let Some(cqe) = self.ring.poll_completion() {
            self.record(cqe);
        }
    }

    fn record(&mut self, cqe: Cqe) {
        self.pending.insert(cqe.user_data, cqe.res);
    }

    /// Poll a future to completion, blocking the calling thread until it
    /// resolves. Drives the ring with a blocking `ring_enter` so deferred
    /// completions make progress (SLOPRING § 7.1/§ 8.3).
    pub fn block_on(&mut self, fut: &mut CompletionFuture) -> Result<i32, RingError> {
        // Already resolved (e.g. a prior `poll` consumed its CQE)? Return
        // the cached result instead of blocking forever on a completion
        // that will never be re-posted.
        if let Some(res) = fut.resolved {
            return Ok(res);
        }
        loop {
            // First, see if its completion already arrived.
            self.drain_cq();
            if let Some(res) = self.pending.remove(&fut.user_data) {
                fut.resolved = Some(res);
                return Ok(res);
            }
            // Block once to drive deferred completions, then re-drain.
            self.ring.submit_and_wait(1)?;
            self.drain_cq();
            if let Some(res) = self.pending.remove(&fut.user_data) {
                fut.resolved = Some(res);
                return Ok(res);
            }
        }
    }

    /// Non-blocking poll: returns `Some(res)` if the future has resolved,
    /// draining the CQ first. Inline completions only (SLOPRING § 8.3).
    pub fn poll(&mut self, fut: &mut CompletionFuture) -> Option<i32> {
        self.drain_cq();
        if let Some(res) = self.pending.remove(&fut.user_data) {
            fut.resolved = Some(res);
            return Some(res);
        }
        None
    }

    /// Cancel an unresolved future's op. Submits an `OP_CANCEL`
    /// SQE targeting the future's `user_data`. This is where async
    /// cancellation belongs — a stuck op degrades one process, never the
    /// kernel.
    pub fn cancel(&mut self, fut: &CompletionFuture) -> Result<(), RingError> {
        if fut.is_ready() {
            return Ok(());
        }
        let mut cancel = Sqe::ZERO;
        cancel.opcode = OP_CANCEL;
        cancel.fd = fut.fd;
        cancel.addr = fut.user_data; // target the in-flight op's cookie
        cancel.user_data = self.alloc_cookie();
        self.ring.push_sqe(&cancel)?;
        self.ring.submit()?;
        Ok(())
    }
}
