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
