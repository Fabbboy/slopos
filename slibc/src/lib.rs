//! slibc — SlopOS Rust-native C standard library.
//! Every call through slibc is a gamble with the Wheel of Fate.

#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![feature(sync_unsafe_cell)]

pub mod crt;
pub mod error;
pub mod ffi;
pub mod mem;
pub mod pal;
pub mod string;

pub use error::{SyscallError, SyscallResult, demux, mux};
pub use mem::malloc::{alloc, calloc, dealloc, realloc};
pub use string::{
    ptr_is_null, slice_from_cstr, slice_from_cstr_mut, u_memcpy, u_memset, u_strlen, u_strnlen,
};
