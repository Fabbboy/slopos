#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod bootstrap;
pub mod loader;
pub mod roulette;
pub mod runtime;
pub mod shell;
pub mod syscall;

pub fn init() {
    // Userland init remains lightweight; boot steps registered via bootstrap.
}
