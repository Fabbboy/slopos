//! slibc — SlopOS Rust-native C standard library.

#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![feature(sync_unsafe_cell)]
#![feature(c_variadic)]

pub mod alloc;
pub mod crt;
pub mod env;
pub mod errno;
pub mod error;
pub mod ffi;
pub mod io;
pub mod mem;
pub mod net;
pub mod pal;
pub mod process;
pub mod signal;
pub mod stdio;
pub mod string;
pub mod test_harness;
pub mod thread;
pub mod time;
pub mod tty;

pub use errno::{__errno_location, Errno, errno_get, errno_set};
pub use error::{SyscallError, SyscallResult, demux, mux};
pub use mem::malloc::{alloc, calloc, dealloc, memalign, realloc};
pub use string::{
    ptr_is_null, slice_from_cstr, slice_from_cstr_mut, u_memcpy, u_memset, u_strlen, u_strnlen,
};
