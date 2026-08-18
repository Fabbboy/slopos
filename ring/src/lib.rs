#![no_std]
#![forbid(unsafe_code)]

//! SlopRing — io_uring-style submission/completion ring (kernel side).
//!
//! Sync only: there is no `async fn` here — `scripts/check_no_kernel_async.sh`
//! enforces it (AD-8/AD-9/R13). Async lives in the userland runtime that drives
//! this ring.

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
