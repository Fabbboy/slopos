use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};

use crate::socket;

pub struct SocketFileOps;

pub static SOCKET_FILE_OPS: SocketFileOps = SocketFileOps;

impl FileOps for SocketFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Socket
    }

    fn read(
        &self,
        handle: usize,
        buf: &mut dyn slopos_abi::io::IoBuf,
        _offset: u64,
        _flags: u32,
    ) -> isize {
        // Socket API uses raw pointers; use a kernel-side staging buffer.
        let mut tmp = [0u8; 4096];
        let read_len = buf.len().min(tmp.len());
        let n = socket::socket_recv(handle as u32, tmp.as_mut_ptr(), read_len);
        if n <= 0 {
            return n as isize;
        }
        match buf.write_at(0, &tmp[..n as usize]) {
            Ok(written) => written as isize,
            Err(e) => e as isize,
        }
    }

    fn write(
        &self,
        handle: usize,
        buf: &mut dyn slopos_abi::io::IoBuf,
        _offset: u64,
        _flags: u32,
    ) -> isize {
        // Socket API uses raw pointers; use a kernel-side staging buffer.
        let mut tmp = [0u8; 4096];
        let write_len = buf.len().min(tmp.len());
        match buf.read_at(0, &mut tmp[..write_len]) {
            Ok(n) => socket::socket_send(handle as u32, tmp.as_ptr(), n) as isize,
            Err(e) => e as isize,
        }
    }

    fn release(&self, handle: usize) {
        let _ = socket::socket_close(handle as u32);
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let socket_idx = handle as u32;
        let readable = socket::socket_poll_readable(socket_idx) as u16;
        let writable = socket::socket_poll_writable(socket_idx) as u16;
        let mut revents = 0u16;

        if (events & POLLIN) != 0 {
            if (readable & 1) != 0 {
                revents |= POLLIN;
            }
            revents |= readable & (POLLIN | POLLERR | POLLHUP);
        }
        if (events & POLLOUT) != 0 {
            if (writable & 1) != 0 {
                revents |= POLLOUT;
            }
            revents |= writable & (POLLOUT | POLLERR | POLLHUP);
        }

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
