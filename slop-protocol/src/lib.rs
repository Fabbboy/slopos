//! SlopOS display protocol — typed enum wire format.
//!
//! All messages are `Request` (client-to-server) or `Event` (server-to-client)
//! enum variants with automatic binary codec. No object map, no manual opcodes.

#![no_std]

extern crate alloc;

pub mod client;
pub mod codec;
pub mod connection;
pub mod server;
pub mod types;

pub use client::Client;
pub use codec::{Decode, Encode};
pub use connection::Connection;
pub use server::Server;
pub use types::*;

/// Thin wrapper around the poll syscall for use by Connection.
pub(crate) fn raw_poll(
    pfd: &mut slopos_abi::syscall::types::UserPollFd,
    nfds: u32,
    timeout_ms: i64,
) -> i32 {
    unsafe {
        slopos_slibc::pal::raw::syscall3(
            slopos_abi::syscall::numbers::SYSCALL_POLL,
            pfd as *mut _ as u64,
            nfds as u64,
            timeout_ms as u64,
        ) as i32
    }
}

/// Monotonic timestamp in milliseconds since boot (for deadline tracking).
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
