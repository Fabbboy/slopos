use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};
use slopos_ostd::KArc;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};

use crate::socket;

pub struct SocketFileOps;

pub static SOCKET_FILE_OPS: SocketFileOps = SocketFileOps;

/// Sole owner of one AF_INET socket; dropping it closes the socket.
#[derive(slopos_ostd::Charged)]
struct SocketBacking {
    idx: u32,
    object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(SocketBacking);

impl FileBacking for SocketBacking {}

impl Drop for SocketBacking {
    fn drop(&mut self) {
        let _ = socket::socket_close(self.idx);
    }
}

/// Wrap ownership of a freshly-created AF_INET socket. On allocation
/// failure the socket is closed before returning, so it cannot leak.
pub fn socket_backing(idx: u32, account: AccountId) -> Option<KArc<dyn FileBacking>> {
    let Ok(reservation) = try_charge::<ObjectRow>(account, 1) else {
        let _ = socket::socket_close(idx);
        return None;
    };
    match KArc::try_new(SocketBacking {
        idx,
        object_charge: Charge::commit(reservation),
    }) {
        Ok(backing) => Some(backing),
        Err(_) => {
            let _ = socket::socket_close(idx);
            None
        }
    }
}

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
        let n = socket::socket_recv(handle as u32, &mut tmp[..read_len]);
        if n <= 0 {
            return n as isize;
        }
        // Defensive clamp: a driver over-report must not overrun the slice.
        let n = (n as usize).min(read_len);
        // Following the documented Linux TCP behaviour, data is consumed from
        // the receive queue before the copy out, so a faulting user buffer
        // loses those bytes.
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
            let sent = socket::socket_send(handle as u32, &staging[..n]);
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

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        // Register FIRST, then check readiness.
        let socket_idx = handle as u32;
        let mut registered = socket::socket_poll_enqueue_recv(socket_idx);
        // Subscribing SEND on POLLOUT is what wakes a write that hit -EAGAIN
        // on a full TX buffer once the TX drain publishes sock_send_ev.
        if (events & POLLOUT) != 0 {
            registered |= socket::socket_poll_enqueue_send(socket_idx);
        }
        let revents = self.poll_events(handle, events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let socket_idx = handle as u32;
        let readable = socket::socket_poll_readable(socket_idx) as u16;
        let writable = socket::socket_poll_writable(socket_idx) as u16;
        let mut revents = 0u16;

        // `readable` / `writable` are bitmaps keyed on the POSIX
        // POLLIN/POLLOUT/POLLERR/POLLHUP values, so each mask must be that
        // value and never `1`.
        if (events & POLLIN) != 0 && (readable & POLLIN) != 0 {
            revents |= POLLIN;
        }
        if (events & POLLOUT) != 0 && (writable & POLLOUT) != 0 {
            revents |= POLLOUT;
        }

        // Per POSIX, POLLERR and POLLHUP are returned whether requested or not.
        revents |= readable & (POLLERR | POLLHUP);
        revents |= writable & (POLLERR | POLLHUP);

        revents
    }

    fn poll_wait(&self, handle: usize) -> bool {
        socket::socket_poll_enqueue_recv(handle as u32)
    }

    fn poll_unwait(&self, handle: usize) {
        // `poll_unwait` carries no `events` mask, so dequeue both; removing an
        // event that was never subscribed is a no-op.
        socket::socket_poll_dequeue_recv(handle as u32);
        socket::socket_poll_dequeue_send(handle as u32);
    }

    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        socket::socket_set_nonblocking(
            handle as u32,
            (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0,
        )
    }
}
