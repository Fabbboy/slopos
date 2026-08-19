//! SlopOS Operating-System Trusted Domain (OSTD): the kernel's trusted core,
//! holding every line of `unsafe` in the kernel behind safe APIs.

// rustc lints `allow_internal_unsafe` itself under `unsafe_code`, so the macros
// that expand `unsafe` into `#![forbid(unsafe_code)]` crates can only be defined
// here — which keeps the injector set greppable.
#![feature(allow_internal_unsafe)]
#![allow(internal_features)]
#![no_std]
#![feature(
    allocator_api,
    coerce_unsized,
    layout_for_ptr,
    sync_unsafe_cell,
    unsize
)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
// Self-alias so `#[derive(Pod)]` / `#[derive(Zeroable)]` expansions, which name
// `::slopos_ostd::…`, resolve inside this crate itself.
extern crate self as slopos_ostd;

mod abi_pod;
mod abi_zeroable;

pub mod acpi;
pub mod arch;
pub mod atomic_bitmap;
pub mod authority;
pub mod bitmap;
pub mod bitmap_slice;
pub mod boot;
pub mod boot_flags;
pub mod boot_info;
pub mod cpu;
pub mod dev;
pub mod dma;
pub mod early_console;
pub mod fblog;
pub mod ffi;
pub mod handle;
pub mod io;
pub mod irq;
pub mod kconsole;
pub mod kdiag;
pub mod klog;
pub mod ksym;
pub mod mm;
pub mod numfmt;
pub mod panic;
pub mod panic_recovery;
pub mod pci;
pub mod platform;
pub mod process;
pub mod ring_buffer;
pub mod seat;
pub mod stacktrace;
pub mod string;
pub mod sync;
pub mod task;
pub mod test_support;
pub mod tx_reclaim;
pub mod uefi;
pub mod unwind;
pub mod user;
pub mod util;
pub mod watchdog;
pub mod wl_currency;

#[doc(hidden)]
pub use paste as __paste;

/// Plain-old-data marker trait.
pub use mm::Pod;
pub use slopos_ostd_derive::{Charged, Pod, SlotFields, Zeroable};

pub use mm::{
    AllocError, Field, FrameAlloc, FrameAllocOptions, HasFields, Init, InitClosure, Initialised,
    KArc, KBTreeMap, KBox, KVec, KVecDeque, KWeak, KernelHeap, PinBox, Slab, SlotPtr, VmReader,
    VmWriter, Zeroable, boxed_zeroed, init_from_closure, init_struct_with, init_zeroed, raw_alloc,
    raw_dealloc,
};

pub use user::{
    UserBytes, UserContext, UserCopyError, UserMode, UserPtr, UserPtrError, UserRegs, UserSlice,
    UserVirtAddr,
};

pub use slopos_abi::alignment;
pub use slopos_abi::alignment::{
    align_down_u64, align_down_usize, align_down_usize as align_down, align_up_u64, align_up_usize,
    align_up_usize as align_up,
};

pub use atomic_bitmap::AtomicBitmap;
pub use bitmap::{Bitmap, words_for};
pub use handle::{Handle, HandleError, HandleTable};
pub use kdiag::{
    KDIAG_STACK_TRACE_DEPTH, kdiag_dump_interrupt_frame, kdiag_dump_lock_graph,
    kdiag_stack_word_at, kdiag_timestamp,
};
pub use klog::{
    KlogBackend, KlogLevel, klog_force_restore_default, klog_get_level, klog_init, klog_is_enabled,
    klog_register_backend, klog_set_level, klog_swap_backend,
};
pub use ring_buffer::RingBuffer;
pub use stacktrace::StacktraceEntry;
pub use tx_reclaim::{TxReclaimToken, ZcNotifToken};
