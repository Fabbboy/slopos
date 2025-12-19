#![no_std]

pub mod fileio;
pub mod ramfs;
pub mod tests;
#[cfg(test)]
extern crate std;

pub use fileio::*;
pub use ramfs::*;
