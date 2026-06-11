#![no_std]
#![forbid(unsafe_code)]

//! SlopRing — io_uring-style submission/completion ring (kernel side).
//!
//! The public SlopRing docs describe the full design. This crate is a
//! `#![forbid(unsafe_code)]` non-OSTD kernel crate that hosts the SQ/CQ
//! snapshot logic, opcode dispatch, the in-flight table, and the
//! per-ring serialization lock. All memory access to ring pages goes
//! through the bounded volatile/atomic `UFrame` accessor OSTD exposes
//! (the only new OSTD `unsafe`); the ring crate itself contains none.
//!
//! **Sync only.** There is no `async fn` here — `scripts/check_no_kernel_async.sh`
//! enforces it (AD-8/AD-9/R13). Async lives entirely in the userland
//! runtime that drives this ring.
//!
//! ## Public surface
//!
//! - [`ring_setup`] / [`ring_enter`] — the two syscall cores, called by
//!   the `core` syscall handlers.
//! - [`file_ops::RING_FILE_OPS`] — the `FileKind::Ring` vtable.

pub mod buffers;
pub mod enter;
pub mod file_ops;
mod net_glue;
mod opcode;
mod region;
pub mod register;
mod registry;
mod ring_obj;

#[cfg(feature = "test-hooks")]
pub mod tests;

pub use enter::{ring_enter, ring_setup};
pub use file_ops::RING_FILE_OPS;
pub use register::{
    ring_register_buffers, ring_register_pbuf_ring, ring_unregister_buffers,
    ring_unregister_pbuf_ring,
};
