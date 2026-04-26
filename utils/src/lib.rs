#![no_std]
#![feature(sync_unsafe_cell)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod alignment;
pub mod atomic_bitmap;
pub mod bitmap;
pub mod bitmap_slice;
pub mod boot_flags;
pub mod boot_info;
pub mod clock;
pub mod io;
pub mod kdiag;
pub mod klog;
pub mod memory;
pub mod numfmt;
pub mod panic_recovery;
pub mod ports;
pub mod ring_buffer;
pub mod stacktrace;
pub mod string;
pub mod wl_currency;

pub use alignment::{align_down_u64, align_down_usize, align_up_u64, align_up_usize};
pub use alignment::{align_down_usize as align_down, align_up_usize as align_up};
pub use atomic_bitmap::AtomicBitmap;
pub use bitmap::words_for;
pub use bitmap::Bitmap;
pub use kdiag::kdiag_dump_interrupt_frame;
pub use kdiag::{kdiag_timestamp, KDIAG_STACK_TRACE_DEPTH};
pub use klog::{
    klog_force_restore_default, klog_get_level, klog_init, klog_is_enabled, klog_register_backend,
    klog_set_level, klog_swap_backend, KlogBackend, KlogLevel,
};
pub use ring_buffer::RingBuffer;
pub use stacktrace::StacktraceEntry;
