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
pub mod arch;
pub mod damage;
pub mod display;
pub mod draw;
pub mod draw_primitives;
pub mod error;
pub mod fate;
pub mod font;
pub mod font_render;
pub mod fs;
pub mod input;
pub mod pixel;
pub mod shm;
pub mod surface;
pub mod syscall;
pub mod task;
pub mod video_traits;
pub mod window;

pub use addr::*;
pub use damage::{DamageRect, DamageTracker, MAX_DAMAGE_REGIONS};
pub use display::{DisplayInfo, FramebufferData};
pub use draw::{DamageTracking, DrawTarget};
pub use error::*;
pub use fate::FateResult;
pub use fs::*;
pub use input::*;
pub use pixel::*;
pub use shm::*;
pub use surface::*;
pub use syscall::*;
pub use task::*;
pub use video_traits::*;
pub use window::*;
