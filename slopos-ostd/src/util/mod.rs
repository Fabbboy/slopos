//! Small typed-safety helpers consumed by kernel-half crates.
//!
//! Each submodule wraps a narrow unsafe primitive (raw-pointer
//! reborrow, `CStr::from_ptr`, unaligned packed-struct read) behind
//! a safe API so consumers never write `unsafe { … }` themselves.

pub mod byte_view;
pub mod callback_ctx;
pub mod cstr;
pub mod packed_view;
