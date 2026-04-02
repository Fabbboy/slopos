//! FileOps implementation for AF_UNIX sockets.
//!
//! Follows the same pattern as `socket_file_ops.rs` (SocketFileOps).
//!
//! The `handle` stored in the open-file entry is the unix socket slot index
//! with bit 31 set (`UNIX_HANDLE_TAG`).  Every method strips the tag before
//! forwarding to `unix_socket::*`.

use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};

use crate::unix_socket;

/// Tag bit set on AF_UNIX socket handles to distinguish them from IP sockets.
/// Shared with `core::syscall::net_handlers` via this public constant.
pub const UNIX_HANDLE_TAG: u32 = 0x8000_0000;

/// Strip the tag and return the raw unix socket slot index.
fn raw_idx(handle: usize) -> u32 {
    (handle as u32) & !UNIX_HANDLE_TAG
}

pub struct UnixSocketFileOps;

pub static UNIX_SOCKET_FILE_OPS: UnixSocketFileOps = UnixSocketFileOps;

impl FileOps for UnixSocketFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Socket
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        let idx = raw_idx(handle);
        let mut tmp = [0u8; IO_STAGING_SIZE];
        let read_len = buf.len().min(tmp.len());
        let n = unix_socket::unix_recv(idx, tmp.as_mut_ptr(), read_len);
        if n <= 0 {
            return n as isize;
        }
        let n = (n as usize).min(read_len);
        match buf.copy_in(0, &tmp[..n]) {
            Ok(written) => written as isize,
            Err(e) => e.as_isize(),
        }
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        let idx = raw_idx(handle);
        let mut staging = [0u8; IO_STAGING_SIZE];
        let buf_len = buf.len();
        let mut total = 0usize;

        while total < buf_len {
            let chunk = (buf_len - total).min(staging.len());
            let n = match buf.copy_out(total, &mut staging[..chunk]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.as_isize()
                    };
                }
            };
            let sent = unix_socket::unix_send(idx, staging.as_ptr(), n);
            if sent <= 0 {
                return if total > 0 {
                    total as isize
                } else {
                    sent as isize
                };
            }
            total += sent as usize;
            if (sent as usize) < n {
                break;
            }
        }
        total as isize
    }

    fn release(&self, handle: usize) {
        let _ = unix_socket::unix_close(raw_idx(handle));
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let (revents, registered) = unix_socket::unix_poll_fused(raw_idx(handle), events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        unix_socket::unix_poll_events(raw_idx(handle), events)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        unix_socket::unix_poll_register(raw_idx(handle))
    }

    fn poll_unwait(&self, handle: usize) {
        unix_socket::unix_poll_unregister(raw_idx(handle));
    }

    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        unix_socket::unix_set_nonblocking(
            raw_idx(handle),
            (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0,
        )
    }
}
