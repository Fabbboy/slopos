#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

// Bootstrap and loader are only used by the kernel, not by standalone binaries
#[cfg(not(feature = "standalone-bin"))]
pub mod bootstrap;
#[cfg(not(feature = "standalone-bin"))]
pub mod loader;

pub mod apps;
pub mod compositor;
pub mod gfx;
pub mod libslop;
pub mod roulette;
pub mod runtime;
pub mod shell;
pub mod syscall;
pub mod theme;
pub mod ui_utils;

/// Initializes userland runtime and registers lightweight startup steps.
///
/// This function performs minimal crate-level setup required to prepare the userland
/// runtime for operation.
///
/// # Examples
///
/// ```
/// userland::init();
/// ```
pub fn init() {
    // Userland init remains lightweight; boot steps registered via bootstrap.
}
