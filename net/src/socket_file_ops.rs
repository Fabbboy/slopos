use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};

use crate::socket;

pub struct SocketFileOps;

pub static SOCKET_FILE_OPS: SocketFileOps = SocketFileOps;

impl FileOps for SocketFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Socket
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        let mut tmp = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let read_len = buf.len().min(tmp.len());
        let n = socket::socket_recv(handle as u32, tmp.as_mut_ptr(), read_len);
        if n <= 0 {
            return n as isize;
        }
        // Clamp to requested length defensively — the socket driver must
        // not return more than `read_len`, but a kernel panic from a
        // slice overrun is never acceptable.
        let n = (n as usize).min(read_len);
        // Linux TCP model: data is consumed from the receive queue before
        // the copy to userspace.  If copy_to_user faults, the bytes are
        // lost — this is acceptable because the calling process supplied
        // a bogus buffer.  See net/ipv4/tcp.c:tcp_recvmsg_locked().
        match buf.copy_in(0, &tmp[..n]) {
            Ok(written) => written as isize,
            Err(e) => e.as_isize(),
        }
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
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
            let sent = socket::socket_send(handle as u32, staging.as_ptr(), n);
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
        let _ = socket::socket_close(handle as u32);
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        // Register FIRST, then check readiness (Linux pattern).
        let registered = self.poll_wait(handle);
        let revents = self.poll_events(handle, events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let socket_idx = handle as u32;
        let readable = socket::socket_poll_readable(socket_idx) as u16;
        let writable = socket::socket_poll_writable(socket_idx) as u16;
        let mut revents = 0u16;

        if (events & POLLIN) != 0 && (readable & 1) != 0 {
            revents |= POLLIN;
        }
        if (events & POLLOUT) != 0 && (writable & 1) != 0 {
            revents |= POLLOUT;
        }

        // Per POSIX, POLLERR and POLLHUP are returned regardless of
        // whether they were requested in `events`.
        revents |= readable & (POLLERR | POLLHUP);
        revents |= writable & (POLLERR | POLLHUP);

        revents
    }

    fn poll_wait(&self, handle: usize) -> bool {
        socket::socket_poll_enqueue_recv(handle as u32)
    }

    fn poll_unwait(&self, handle: usize) {
        socket::socket_poll_dequeue_recv(handle as u32);
    }

    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        socket::socket_set_nonblocking(
            handle as u32,
            (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0,
        )
    }
}
