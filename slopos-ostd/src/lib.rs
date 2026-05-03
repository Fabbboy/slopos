//! SlopOS Operating-System Trusted Domain (OSTD).
//!
//! This crate is the kernel's trusted core: every line of `unsafe`
//! in the kernel lives here. All other kernel crates consume the
//! safe APIs exposed from this crate.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod arch;
pub mod boot;
pub mod cpu;
pub mod io;
pub mod irq;
pub mod mm;
pub mod sync;
pub mod task;
pub mod user;

/// Plain-old-data marker trait. Re-exported at the crate root so
/// the `#[derive(Pod)]` expansion can resolve `::slopos_ostd::Pod`.
pub use mm::Pod;
pub use slopos_ostd_derive::Pod;

pub use user::{
    UserBytes, UserContext, UserCopyError, UserMode, UserPtr, UserPtrError, UserRegs, UserSlice,
    UserVirtAddr,
};
