//! FileOps implementation for AF_UNIX sockets.
//!
//! The `handle` stored in the open-file entry is a [`SocketHandle`] encoded
//! as `usize`.  Every method reconstructs the handle at the boundary via
//! `SocketHandle::from_usize(handle)` before forwarding to `unix_socket::*`.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};

use crate::unix_socket;
use crate::unix_socket::SocketHandle;

pub struct UnixSocketFileOps;

pub static UNIX_SOCKET_FILE_OPS: UnixSocketFileOps = UnixSocketFileOps;

impl FileOps for UnixSocketFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Socket
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        let h = SocketHandle::from_usize(handle);
        let mut staging = match slopos_alloc::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let buf_len = buf.len();
        let mut total = 0usize;

        while total < buf_len {
            let chunk = (buf_len - total).min(staging.len());
            let n = unix_socket::unix_recv(h, &mut staging[..chunk]);
            if n < 0 {
                return if total > 0 {
                    total as isize
                } else {
                    n as isize
                };
            }
            if n == 0 {
                break; // EOF
            }
            let n = n as usize;
            match buf.copy_in(total, &staging[..n]) {
                Ok(written) => total += written,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.as_isize()
                    };
                }
            }
            if n < chunk {
                break; // Short read — don't loop to avoid blocking
            }
        }
        total as isize
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        let h = SocketHandle::from_usize(handle);
        let mut staging = match slopos_alloc::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
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
            let sent = unix_socket::unix_send(h, &staging[..n]);
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
        let _ = unix_socket::unix_close(SocketHandle::from_usize(handle));
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = SocketHandle::from_usize(handle);
        let (revents, registered) = unix_socket::unix_poll_fused(h, events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        unix_socket::unix_poll_events(SocketHandle::from_usize(handle), events)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        unix_socket::unix_poll_register(SocketHandle::from_usize(handle))
    }

    fn poll_unwait(&self, handle: usize) {
        unix_socket::unix_poll_unregister(SocketHandle::from_usize(handle));
    }

    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        unix_socket::unix_set_nonblocking(
            SocketHandle::from_usize(handle),
            (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0,
        )
    }
}
