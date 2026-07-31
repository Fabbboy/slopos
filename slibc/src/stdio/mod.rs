//! stdio — buffered I/O and the sacred printf.
//!
//! The Scroll of Output — every formatted byte is a spin of the Wheel of Fate.

use core::ptr;

use crate::pal::Pal;

pub mod chars;
pub mod file;
pub mod lock;
pub mod printf;
pub mod registry;
pub mod scanf;
pub mod shim;
pub mod streams;
pub mod tests;

pub use registry::WalkMode;
pub use streams::{stderr, stdin, stdout};

use lock::StreamLock;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// End-of-file sentinel.
pub const EOF: i32 = -1;

/// Seek from beginning of file.
pub const SEEK_SET: i32 = 0;
/// Seek from current position.
pub const SEEK_CUR: i32 = 1;
/// Seek from end of file.
pub const SEEK_END: i32 = 2;

/// Full buffering mode.
pub const _IOFBF: i32 = 0;
/// Line buffering mode.
pub const _IOLBF: i32 = 1;
/// No buffering mode.
pub const _IONBF: i32 = 2;

/// FILE flag: end-of-file reached.
pub const FILE_FLAG_EOF: u32 = 1;
/// FILE flag: I/O error occurred.
pub const FILE_FLAG_ERR: u32 = 2;
/// FILE flag: stream is readable.
pub const FILE_FLAG_READABLE: u32 = 4;
/// FILE flag: stream is writable.
pub const FILE_FLAG_WRITABLE: u32 = 8;
/// FILE flag: fd should be closed on fclose.
pub const FILE_FLAG_OWNED_FD: u32 = 16;
/// FILE flag: the most recent operation was input, so the buffer holds
/// read-ahead and the fd offset is ahead of the stream position.
pub const FILE_FLAG_READING: u32 = 32;
/// FILE flag: the most recent operation was output, so the buffer holds
/// unwritten bytes and the fd offset is behind the stream position.
pub const FILE_FLAG_WRITING: u32 = 64;
/// FILE flag: the stream is on the open-stream list.
pub const FILE_FLAG_LINKED: u32 = 128;
/// FILE flag: the `FILE` itself was allocated by `fopen`/`fdopen` and must be
/// released on `fclose`.
pub const FILE_FLAG_HEAP: u32 = 256;

/// Internal buffer size for FILE streams.
pub const BUFSIZ: usize = 4096;

// ---------------------------------------------------------------------------
// BufferMode
// ---------------------------------------------------------------------------

/// Buffering strategy for a FILE stream.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum BufferMode {
    /// Fully buffered — flush when buffer is full.
    Full = 0,
    /// Line buffered — flush on newline or when buffer is full.
    Line = 1,
    /// Unbuffered — every write goes directly to the fd.
    None = 2,
}

// ---------------------------------------------------------------------------
// FILE
// ---------------------------------------------------------------------------

/// The FILE stream abstraction — every read and write is a gamble.
///
/// One buffer serves both directions, so which direction it currently holds is
/// state: `FILE_FLAG_READING` and `FILE_FLAG_WRITING` say whose bytes are in
/// `buf`, and [`FILE::to_read`] / [`FILE::to_write`] are the only transitions
/// between them. C11 §7.21.5.3 makes the transition the program's business on
/// an update stream; making it the stream's business here means neither
/// direction can silently eat the other's bytes.
#[repr(C)]
pub struct FILE {
    /// File descriptor.
    pub fd: i32,
    /// Current position in the buffer (read or write cursor).
    pub buf_pos: usize,
    /// Number of valid bytes in buffer (meaningful for read buffers).
    pub buf_len: usize,
    /// Stream state flags (EOF, ERR, READABLE, WRITABLE, OWNED_FD, direction,
    /// list membership, allocation ownership).
    pub flags: u32,
    /// Buffering mode.
    pub mode: BufferMode,
    /// `ungetc` push-back slot (−1 = empty, 0–255 = pushed-back byte).
    pub ungot: i32,
    /// Next stream on the open-stream list, or null.
    pub next: *mut FILE,
    /// Recursive per-stream lock (POSIX §2.5.1).
    pub lock: StreamLock,
    /// Internal I/O buffer. Last, so the scalars share cache lines.
    pub buf: [u8; BUFSIZ],
}

#[allow(non_camel_case_types)]
pub type FILE_t = FILE;

impl FILE {
    /// Construct a FILE at compile time — used for the three standard streams.
    pub const fn new_const(fd: i32, mode: BufferMode, flags: u32) -> FILE {
        FILE {
            fd,
            buf_pos: 0,
            buf_len: 0,
            flags,
            mode,
            ungot: -1,
            next: ptr::null_mut(),
            lock: StreamLock::new(),
            buf: [0u8; BUFSIZ],
        }
    }

    /// Initialise a FILE in place. Writing the fields through the destination
    /// pointer keeps the 4 KiB buffer off the caller's stack.
    ///
    /// # Safety
    /// `dst` must point at writable, suitably aligned storage of at least
    /// `size_of::<FILE>()` bytes.
    pub unsafe fn init_at(dst: *mut FILE, fd: i32, mode: BufferMode, flags: u32) {
        ptr::write(&raw mut (*dst).fd, fd);
        ptr::write(&raw mut (*dst).buf_pos, 0);
        ptr::write(&raw mut (*dst).buf_len, 0);
        ptr::write(&raw mut (*dst).flags, flags);
        ptr::write(&raw mut (*dst).mode, mode);
        ptr::write(&raw mut (*dst).ungot, -1);
        ptr::write(&raw mut (*dst).next, ptr::null_mut());
        ptr::write(&raw mut (*dst).lock, StreamLock::new());
        ptr::write_bytes(&raw mut (*dst).buf as *mut u8, 0, BUFSIZ);
    }

    /// Flush the write buffer to the underlying fd.
    ///
    /// Returns 0 on success, [`EOF`] on error.
    pub fn flush_write_buf(&mut self) -> i32 {
        if self.buf_pos == 0 {
            return 0;
        }
        let mut written = 0usize;
        while written < self.buf_pos {
            match crate::pal::Sys::write(
                self.fd,
                self.buf[written..].as_ptr(),
                self.buf_pos - written,
            ) {
                Ok(n) => {
                    if n == 0 {
                        self.flags |= FILE_FLAG_ERR;
                        return EOF;
                    }
                    written += n;
                }
                Err(_) => {
                    self.flags |= FILE_FLAG_ERR;
                    return EOF;
                }
            }
        }
        self.buf_pos = 0;
        0
    }

    /// Refill the read buffer from the underlying fd.
    ///
    /// Returns the number of bytes read, or [`EOF`] on error / end-of-file.
    pub fn fill_read_buf(&mut self) -> i32 {
        self.buf_pos = 0;
        self.buf_len = 0;
        match crate::pal::Sys::read(self.fd, self.buf.as_mut_ptr(), BUFSIZ) {
            Ok(n) => {
                if n == 0 {
                    self.flags |= FILE_FLAG_EOF;
                    return EOF;
                }
                self.buf_len = n;
                n as i32
            }
            Err(_) => {
                self.flags |= FILE_FLAG_ERR;
                EOF
            }
        }
    }

    /// Bytes buffered but not yet consumed by the program, including the
    /// `ungetc` slot. The fd offset is this far ahead of the stream position.
    pub fn read_ahead_len(&self) -> i64 {
        let buffered = self.buf_len.saturating_sub(self.buf_pos) as i64;
        buffered + if self.ungot >= 0 { 1 } else { 0 }
    }

    /// Rewind the fd over unconsumed read-ahead and drop it.
    ///
    /// Best effort: `ESPIPE` on a pipe or a terminal is the correct answer, and
    /// the read-ahead is dropped either way.
    pub fn discard_read_ahead(&mut self) {
        let ahead = self.read_ahead_len();
        if ahead > 0 {
            let _ = crate::pal::Sys::lseek(self.fd, -ahead, SEEK_CUR);
        }
        self.buf_pos = 0;
        self.buf_len = 0;
        self.ungot = -1;
    }

    /// Enter the input direction. Returns `false` if a pending write could not
    /// be delivered, in which case the caller must not read.
    pub fn to_read(&mut self) -> bool {
        if self.flags & FILE_FLAG_WRITING != 0 {
            if self.flush_write_buf() == EOF {
                return false;
            }
            self.flags &= !FILE_FLAG_WRITING;
            self.buf_len = 0;
        }
        self.flags |= FILE_FLAG_READING;
        true
    }

    /// Enter the output direction, giving back any read-ahead the program never
    /// consumed.
    pub fn to_write(&mut self) -> bool {
        if self.flags & FILE_FLAG_READING != 0 {
            self.discard_read_ahead();
            self.flags &= !(FILE_FLAG_READING | FILE_FLAG_EOF);
        }
        self.flags |= FILE_FLAG_WRITING;
        true
    }
}

// ---------------------------------------------------------------------------
// Process teardown
// ---------------------------------------------------------------------------

/// Flush every open output stream on the way out of the process.
///
/// Bounded: a peer thread wedged in `write()` costs a few milliseconds and is
/// then skipped, rather than turning a clean exit into a hang.
pub fn __stdio_exit() -> i32 {
    registry::flush_all(WalkMode::BestEffort)
}
