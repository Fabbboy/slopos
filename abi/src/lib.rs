//! SlopOS Kernel-Userland ABI Types
//!
//! This crate provides the canonical definitions for all types shared between
//! the kernel and userland. Having a single source of truth eliminates:
//! - Duplicate type definitions
//! - ABI mismatches between kernel and userland
//! - The need for unsafe FFI conversions
//!
//! All types in this crate are `#[repr(C)]` for ABI stability.

#![no_std]
#![forbid(unsafe_code)]

pub mod error;
pub mod input;
pub mod pixel;
pub mod sched_traits;
pub mod shm;
pub mod surface;
pub mod window;

pub use error::*;
pub use input::*;
pub use pixel::*;
pub use sched_traits::*;
pub use shm::*;
pub use surface::*;
pub use window::*;
