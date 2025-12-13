#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod bootstrap;
pub mod loader;
pub mod runtime;
pub mod shell;
pub mod syscall;
pub mod roulette;

pub fn init() {
    // Userland init remains lightweight; boot steps registered via bootstrap.
}

