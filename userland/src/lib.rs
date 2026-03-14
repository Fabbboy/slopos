#![feature(restricted_std)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod appkit;
pub mod apps;
pub mod gfx;
pub mod program_registry;
pub mod runtime;
pub mod syscall;
pub mod theme;
pub mod ui_utils;

pub use slopos_slibc as slibc;

pub fn init() {}
