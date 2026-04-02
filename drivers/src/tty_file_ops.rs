use slopos_abi::Errno;
use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{FS_TYPE_CHARDEV, UserFsStat};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::TtyIndex;

use crate::tty;

pub struct TtyFileOps;

pub static TTY_FILE_OPS: TtyFileOps = TtyFileOps;

fn validated_tty_index(handle: usize) -> Result<TtyIndex, Errno> {
    if handle > u8::MAX as usize {
        return Err(Errno::EBADF);
    }
    Ok(TtyIndex(handle as u8))
}

impl FileOps for TtyFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let tty_idx = match validated_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let mut tmp = [0u8; IO_STAGING_SIZE];
        let read_len = buf.len().min(tmp.len());
        match tty::read(tty_idx, &mut tmp[..read_len], nonblock) {
            Ok(n) => {
                // Clamp defensively — tty::read structurally cannot exceed
                // read_len, but a kernel panic is never acceptable.
                let n = n.min(read_len);
                // Linux TTY model: ldisc.read() is destructive (peek+pop).
                // If copy_to_user faults afterwards, the keystrokes are lost.
                // This matches Linux drivers/tty/n_tty.c behaviour — a process
                // that passes a bogus buffer to read(2) loses the data.
                match buf.copy_in(0, &tmp[..n]) {
                    Ok(written) => written as isize,
                    Err(e) => e.as_isize(),
                }
            }
            Err(e) => e.to_errno() as isize,
        }
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        let tty_idx = match validated_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
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
            match tty::write(tty_idx, &staging[..n], nonblock) {
                Ok(written) => {
                    total += written;
                    if written < n {
                        break;
                    }
                }
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.to_errno() as isize
                    };
                }
            }
        }
        total as isize
    }

    fn release(&self, handle: usize) {
        if let Ok(idx) = validated_tty_index(handle) {
            let _ = tty::close_ref(idx);
        }
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let tty_idx = validated_tty_index(handle).ok()?;
        if tty::open_ref(tty_idx).is_ok() {
            Some(handle)
        } else {
            None
        }
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
        match validated_tty_index(handle) {
            Ok(idx) => tty::poll_events(idx, events),
            Err(_) => 0,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match validated_tty_index(handle) {
            Ok(idx) => tty::poll_enqueue(idx),
            Err(_) => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Ok(idx) = validated_tty_index(handle) {
            tty::poll_dequeue(idx);
        }
    }

    fn stat(&self, _handle: usize, out: &mut UserFsStat) -> i32 {
        out.type_ = FS_TYPE_CHARDEV;
        out.size = 0;
        0
    }
}
