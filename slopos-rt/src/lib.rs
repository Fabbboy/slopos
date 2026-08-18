//! `slopos-rt` — the SlopOS userland async runtime: the userland
//! [`Ring`](ring::Ring) SQ/CQ wrapper, the [`slopfut`] executor/reactor/op
//! stack, and the thin syscall shims those need. This is **userland**: async
//! lives here, never in the kernel (AD-8/AD-9). The kernel side is the
//! strictly-synchronous `ring/` crate driven by the two `ring_*` syscalls.
//!
//! It deps only on `slopos-abi` + `slopos-slibc`, so the shared userland libs
//! (windowing/appkit/compositor) that must later go async can reach it without
//! a dependency cycle.
//!
//! `slopfut`'s `SCHED` (executor.rs) and `REACTOR` (reactor.rs) thread-locals
//! are the per-core seam, and are backed by real per-thread storage (the
//! compiler-native `#[thread_local]` arm over variant-II FS_BASE TLS), so each
//! OS thread that calls `block_on` gets its own scheduler, reactor and `Ring`.

#![feature(restricted_std)]
#![allow(dead_code)]

pub mod ring;
pub mod slopfut;
mod sys;

pub use ring::{Ring, RingError};
