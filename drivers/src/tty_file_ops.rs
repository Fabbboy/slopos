use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{FS_TYPE_CHARDEV, UserFsStat};
use slopos_abi::syscall::TtyIndex;

use crate::tty;

pub struct TtyFileOps;

pub static TTY_FILE_OPS: TtyFileOps = TtyFileOps;

impl FileOps for TtyFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn slopos_abi::io::IoBuf, _offset: u64, flags: u32) -> isize {
        let tty_idx = TtyIndex(handle as u8);
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        // TTY driver API uses &mut [u8]; use a kernel-side staging buffer.
        let mut tmp = [0u8; 4096];
        let read_len = buf.len().min(tmp.len());
        match tty::read(tty_idx, &mut tmp[..read_len], nonblock) {
            Ok(n) => match buf.write_at(0, &tmp[..n]) {
                Ok(written) => written as isize,
                Err(e) => e as isize,
            },
            Err(e) => e.to_errno() as isize,
        }
    }

    fn write(&self, handle: usize, buf: &mut dyn slopos_abi::io::IoBuf, _offset: u64, flags: u32) -> isize {
        let tty_idx = TtyIndex(handle as u8);
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        // TTY driver API uses &[u8]; use a kernel-side staging buffer.
        let mut tmp = [0u8; 4096];
        let write_len = buf.len().min(tmp.len());
        match buf.read_at(0, &mut tmp[..write_len]) {
            Ok(n) => match tty::write(tty_idx, &tmp[..n], nonblock) {
                Ok(written) => written as isize,
                Err(e) => e.to_errno() as isize,
            },
            Err(e) => e as isize,
        }
    }

    fn release(&self, handle: usize) {
        let _ = tty::close_ref(TtyIndex(handle as u8));
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let tty_idx = TtyIndex(handle as u8);
        if tty::open_ref(tty_idx).is_ok() {
            Some(handle)
        } else {
            None
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        tty::poll_events(TtyIndex(handle as u8), events)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        tty::poll_enqueue(TtyIndex(handle as u8))
    }

    fn poll_unwait(&self, handle: usize) {
        tty::poll_dequeue(TtyIndex(handle as u8));
    }

    fn stat(&self, _handle: usize, out: &mut UserFsStat) -> i32 {
        out.type_ = FS_TYPE_CHARDEV;
        out.size = 0;
        0
    }
}
