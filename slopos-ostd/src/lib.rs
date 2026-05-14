//! SlopOS Operating-System Trusted Domain (OSTD).
//!
//! This crate is the kernel's trusted core: every line of `unsafe`
//! in the kernel lives here. All other kernel crates consume the
//! safe APIs exposed from this crate.

#![no_std]
#![feature(allocator_api, coerce_unsized, sync_unsafe_cell, unsize)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod abi_pod;
mod abi_zeroable;

pub mod acpi;
pub mod arch;
pub mod atomic_bitmap;
pub mod bitmap;
pub mod bitmap_slice;
pub mod boot;
pub mod boot_flags;
pub mod boot_info;
pub mod cpu;
pub mod dev;
pub mod dma;
pub mod early_console;
pub mod ffi;
pub mod io;
pub mod irq;
pub mod kdiag;
pub mod klog;
pub mod memory;
pub mod mm;
pub mod numfmt;
pub mod panic_recovery;
pub mod pci;
pub mod ring_buffer;
pub mod stacktrace;
pub mod string;
pub mod sync;
pub mod task;
pub mod test_support;
pub mod user;
pub mod util;
pub mod wl_currency;

#[doc(hidden)]
pub use paste as __paste;

/// Plain-old-data marker trait. Re-exported at the crate root so
/// the `#[derive(Pod)]` / `#[derive(Zeroable)]` expansions can
/// resolve `::slopos_ostd::Pod` / `::slopos_ostd::Zeroable`.
pub use mm::Pod;
pub use slopos_ostd_derive::{Pod, Zeroable};

pub use mm::{
    AllocError, FrameAlloc, FrameAllocOptions, Init, InitClosure, KArc, KBTreeMap, KBox, KVec,
    KVecDeque, KernelHeap, PinBox, Slab, Zeroable, boxed_zeroed, init_from_closure, init_zeroed,
    raw_alloc, raw_dealloc,
};

pub use user::{
    UserBytes, UserContext, UserCopyError, UserMode, UserPtr, UserPtrError, UserRegs, UserSlice,
    UserVirtAddr,
};

// ---------------------------------------------------------------------------
// Convenience re-exports absorbed from slopos-utils.
// ---------------------------------------------------------------------------

pub use slopos_abi::alignment;
pub use slopos_abi::alignment::{
    align_down_u64, align_down_usize, align_down_usize as align_down, align_up_u64, align_up_usize,
    align_up_usize as align_up,
};

pub use atomic_bitmap::AtomicBitmap;
pub use bitmap::{Bitmap, words_for};
pub use kdiag::{
    KDIAG_STACK_TRACE_DEPTH, kdiag_dump_interrupt_frame, kdiag_stack_word_at, kdiag_timestamp,
};
pub use klog::{
    KlogBackend, KlogLevel, klog_force_restore_default, klog_get_level, klog_init, klog_is_enabled,
    klog_register_backend, klog_set_level, klog_swap_backend,
};
pub use ring_buffer::RingBuffer;
pub use stacktrace::StacktraceEntry;
