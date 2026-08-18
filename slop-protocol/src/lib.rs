//! SlopOS display protocol — typed enum wire format.
//!
//! Length-prefixed binary codec, all little-endian.

#![no_std]

extern crate alloc;

pub mod client;
pub mod codec;
pub mod connection;
pub mod server;
pub mod types;

pub use client::Client;
pub use codec::{Decode, Encode, FdFifo};
pub use connection::Connection;
pub use server::Server;
pub use types::*;

/// Monotonic timestamp in milliseconds since boot.
pub(crate) fn timestamp_ms() -> u64 {
    let mut ts = slopos_abi::syscall::types::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        slopos_slibc::pal::raw::syscall2(
            slopos_abi::syscall::numbers::SYSCALL_CLOCK_GETTIME,
            slopos_abi::syscall::CLOCK_MONOTONIC,
            &mut ts as *mut _ as u64,
        );
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}
