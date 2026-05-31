//! `slopos-rt` — the SlopOS userland async runtime.
//!
//! Houses the userland [`Ring`](ring::Ring) SQ/CQ wrapper and the
//! [`slopfut`] executor/reactor/op/select/sync/time/io/signal/process/waker
//! stack, plus the thin syscall shims those need. This is **userland**: async
//! lives here, never in the kernel (AD-8/AD-9). The kernel side is the
//! strictly-synchronous `ring/` crate driven by the two `ring_*` syscalls.
//!
//! It deps only on `slopos-abi` + `slopos-slibc`, so the shared userland libs
//! (windowing/appkit/compositor) that must later go async can reach it without
//! a dependency cycle. `userland` re-exports it as `pub use slopos_rt as ring;`
//! so every existing `crate::ring::{Ring, slopfut}` caller compiles unchanged.
//!
//! ## Phase-6 per-core seam
//!
//! `slopfut`'s `SCHED` (executor.rs) and `REACTOR` (reactor.rs) thread-locals
//! are the per-core seam: a future multi-core runtime makes one executor +
//! reactor per core, so any per-core migration lands at those two statics.
//!
//! As of Tier A, those thread-locals are backed by real per-thread storage:
//! std routes `thread_local!` through the compiler-native (`#[thread_local]`)
//! arm over variant-II FS_BASE TLS (one block per OS thread), not a single
//! process-global cell. So a per-thread `block_on` is now correct — each OS
//! thread that calls it gets its own `SCHED` + `REACTOR` (and its own `Ring`),
//! with no cross-thread aliasing of those statics.
//!
//! Tier B (per-core reactor scale-out) lands the cross-core wakeup-fd plus a
//! `Send` channel that rides it — both additive, leaving the single-threaded
//! `block_on`/reactor path byte-for-byte unchanged:
//!   1. Cross-core wakeup-fd (DONE): a reactor lazily arms an `O_NONBLOCK`
//!      self-pipe with a standing multishot `OP_POLL_ADD`; a `Send` sender on
//!      another core writes a byte that completes that poll in the target
//!      reactor's `park`, which fires the receiver task's local waker. The
//!      pipe is created only when a [`slopfut::cross_core`] receiver is built
//!      on the reactor — a reactor with none never creates it.
//!   2. `Send` cross-core channel (DONE): [`slopfut::cross_core`] — a
//!      `Send + Sync + Clone` sender over `Arc<Mutex<VecDeque>>` + the target
//!      reactor's wakeup-fd; the `!Send` receiver integrates with that reactor.
//!
//! Deferred (no consumer yet, intentionally not built):
//!   - Work-stealing between reactors.
//!   - Per-reactor buffer pools (buffers are per-op today; a per-core runtime
//!     wants strictly per-reactor pools — registered-buffer rings are per-ring).
//!   - Per-thread signalfd mask reconciliation (`rt_sigprocmask` is per-thread,
//!     so each reactor establishes its own signalfd + mask, forcing a decision
//!     between process- and thread-directed signal delivery).
//!   - `futex_wait` timeout enforcement: the cross-core channel's wake path is
//!     the wakeup-fd, so it never needs a timed wait.

#![feature(restricted_std)]
#![allow(dead_code)]

pub mod ring;
pub mod slopfut;
mod sys;

pub use ring::{Ring, RingError};
