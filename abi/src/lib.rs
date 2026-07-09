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

pub mod addr;
pub mod alignment;
pub mod auxv;
pub mod damage;
pub mod display;
pub mod draw;
pub mod errno;
pub mod error;
pub mod event;
pub mod fate;
pub mod file_ops;
pub mod fs;
pub mod handle;
pub mod input;
pub mod io;
pub mod net;
pub mod pixel;
pub mod ring;
pub mod signal;
pub mod spawn;
pub mod surface;
pub mod syscall;
pub mod task;
pub mod tty_error;
pub mod unicode;
pub mod unix;
pub mod video_traits;
pub mod window;

/// Standard 4KB page size for userland memory calculations.
pub const PAGE_SIZE: u64 = 0x1000;

pub use addr::*;
pub use alignment::{align_down_u64, align_down_usize, align_up_u64, align_up_usize};
pub use damage::{DamageRect, MAX_DAMAGE_REGIONS, MAX_INTERNAL_DAMAGE_REGIONS};
pub use display::{DisplayInfo, FramebufferData};
pub use draw::{Canvas, Color32, EncodedPixel};
pub use errno::Errno;
pub use error::*;
pub use event::*;
pub use fate::FateResult;
pub use file_ops::{FileKind, FileOps};
pub use fs::*;
pub use handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle,
};
pub use input::*;
pub use io::{IoBufRead, IoBufWrite, KernelIoBuf, KernelIoBufRef};
pub use net::*;
pub use pixel::*;
pub use surface::*;
pub use syscall::*;
pub use task::*;
pub use tty_error::*;
pub use video_traits::*;
pub use window::*;
