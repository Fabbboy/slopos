//! SlopOS Operating-System Trusted Domain (OSTD).
//!
//! This crate is the kernel's trusted core: every line of `unsafe`
//! in the kernel lives here. All other kernel crates consume the
//! safe APIs exposed from this crate.

#![no_std]
#![feature(allocator_api, coerce_unsized, sync_unsafe_cell, unsize)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod arch;
pub mod boot;
pub mod cpu;
pub mod io;
pub mod irq;
pub mod mm;
pub mod sync;
pub mod task;
pub mod user;
pub mod util;

/// Plain-old-data marker trait. Re-exported at the crate root so
/// the `#[derive(Pod)]` expansion can resolve `::slopos_ostd::Pod`.
pub use mm::Pod;
pub use slopos_ostd_derive::Pod;

pub use mm::{
    AllocError, FrameAlloc, FrameAllocOptions, Init, InitClosure, KArc, KBTreeMap, KBox, KVec,
    KVecDeque, KernelHeap, PinBox, Slab, Zeroable, boxed_zeroed, init_from_closure, init_zeroed,
    raw_alloc, raw_dealloc,
};

pub use user::{
    UserBytes, UserContext, UserCopyError, UserMode, UserPtr, UserPtrError, UserRegs, UserSlice,
    UserVirtAddr,
};
