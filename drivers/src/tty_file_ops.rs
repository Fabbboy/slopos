use slopos_abi::Errno;
use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{FS_TYPE_CHARDEV, UserFsStat};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::TtyIndex;

use crate::tty;

// ---------------------------------------------------------------------------
// TtyHandle — type-safe handle for TTY kernel objects
// ---------------------------------------------------------------------------

/// Lightweight newtype wrapping a TTY index for type-safe passage through
/// the `FileOps` `handle: usize` boundary.  No generation counter needed
/// since TTY slots are fixed (never recycled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct TtyHandle(u8);

impl TtyHandle {
    /// Convert to usize for storage in OpenFileEntry.handle.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from usize stored in OpenFileEntry.handle.
    /// Returns `None` if the value exceeds `u8::MAX`.
    pub fn from_usize(v: usize) -> Option<Self> {
        if v > u8::MAX as usize {
            None
        } else {
            Some(Self(v as u8))
        }
    }

    /// Return the underlying [`TtyIndex`].
    pub fn index(self) -> TtyIndex {
        TtyIndex(self.0)
    }
}

pub struct TtyFileOps;

pub static TTY_FILE_OPS: TtyFileOps = TtyFileOps;

impl FileOps for TtyFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let Some(th) = TtyHandle::from_usize(handle) else {
            return Errno::EBADF.as_isize();
        };
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let mut tmp = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let read_len = buf.len().min(tmp.len());
        match tty::read(th.index(), &mut tmp[..read_len], nonblock) {
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
        let Some(th) = TtyHandle::from_usize(handle) else {
            return Errno::EBADF.as_isize();
        };
        let nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
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
            match tty::write(th.index(), &staging[..n], nonblock) {
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
        if let Some(th) = TtyHandle::from_usize(handle) {
            let _ = tty::close_ref(th.index());
        }
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let th = TtyHandle::from_usize(handle)?;
        if tty::open_ref(th.index()).is_ok() {
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
        match TtyHandle::from_usize(handle) {
            Some(th) => tty::poll_events(th.index(), events),
            None => 0,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match TtyHandle::from_usize(handle) {
            Some(th) => tty::poll_enqueue(th.index()),
            None => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Some(th) = TtyHandle::from_usize(handle) {
            tty::poll_dequeue(th.index());
        }
    }

    fn stat(&self, _handle: usize, out: &mut UserFsStat) -> i32 {
        out.type_ = FS_TYPE_CHARDEV;
        out.size = 0;
        0
    }
}
