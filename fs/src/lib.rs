#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(unused_unsafe)]
#![allow(unused_imports)]
#![allow(unused_mut)]
#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

mod fileio;
mod ramfs;
pub mod tests;
#[cfg(test)]
extern crate std;

pub use fileio::*;
pub use ramfs::*;
