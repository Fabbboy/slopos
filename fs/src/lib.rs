#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod fileio;
mod ramfs;
pub mod tests;
#[cfg(test)]
extern crate std;

pub use fileio::*;
pub use ramfs::*;

