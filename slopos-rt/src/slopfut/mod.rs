//! `slopfut` — a small, production-shaped `async`/`await` runtime over one
//! [`Ring`](super::Ring).
//!
//! This is the real executor the SlopRing "async edge" was missing: a
//! single-threaded scheduler with real wakers ([`executor`]), a reactor that
//! turns ring completions into woken tasks ([`reactor`]), leaf op-futures
//! with ownership-passing buffers ([`op`]), and combinators ([`select`]).
//! On top sit the higher-level surfaces a tokio-class runtime offers:
//! [`time`] (sleep/timeout), [`io`] (async TCP/UDP/file), [`sync`]
//! (Notify/channels), [`signal`] (ctrl_c via signalfd), and [`process`]
//! (Child::wait via pidfd).
//!
//! ## Model
//!
//! SLOPRING §7.1: deferred completions progress only inside a blocking
//! `ring_enter`, so the runtime is single-threaded and its only sleep is the
//! reactor's `park`. [`block_on`] runs a root future to completion while
//! [`spawn`]ed tasks run concurrently on the same thread; a future is polled
//! only when its waker fires (a ring completion, or another task), never by
//! re-polling everything.

use super::Ring;

mod executor;
mod op;
mod reactor;
mod select;
mod waker;

pub mod cross_core;
pub mod io;
pub mod process;
pub mod signal;
pub mod sync;
pub mod time;

pub use executor::{JoinHandle, spawn, yield_now};
pub use op::{
    BufOp, BufResult, IntOp, MultishotStream, RecvFromOp, RecvFromResult, accept, accept_multishot,
    close, nop, openat, poll_add, poll_add_multishot, read, recvfrom, recvmsg_multishot, timeout,
    write,
};
pub use select::{Either2, Either3, Select2, Select3, join2, select2, select3};

/// Drive `fut` to completion on `ring`, running spawned tasks concurrently.
/// Consumes the ring (unmapped + closed on return). Not re-entrant.
pub fn block_on<F: core::future::Future>(ring: Ring, fut: F) -> F::Output {
    executor::block_on(ring, fut)
}
