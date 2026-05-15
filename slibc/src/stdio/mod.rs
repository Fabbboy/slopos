//! stdio — buffered I/O and the sacred printf.
//!
//! The Scroll of Output — every formatted byte is a spin of the Wheel of Fate.

use crate::pal::Pal;

pub mod chars;
pub mod file;
pub mod printf;
pub mod scanf;
#[allow(dead_code)]
pub(crate) mod shim;
pub mod streams;
pub mod tests;

pub use streams::{stderr, stdin, stdout};

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
#[repr(C)]
pub struct FILE {
    /// File descriptor.
    pub fd: i32,
    /// Internal I/O buffer.
    pub buf: [u8; BUFSIZ],
    /// Current position in the buffer (read or write cursor).
    pub buf_pos: usize,
    /// Number of valid bytes in buffer (meaningful for read buffers).
    pub buf_len: usize,
    /// Stream state flags (EOF, ERR, READABLE, WRITABLE, OWNED_FD).
    pub flags: u32,
    /// Buffering mode.
    pub mode: BufferMode,
    /// `ungetc` push-back slot (−1 = empty, 0–255 = pushed-back byte).
    pub ungot: i32,
}

#[allow(non_camel_case_types)]
pub type FILE_t = FILE;

impl FILE {
    /// Construct a FILE at compile time — used for the three standard streams.
    pub const fn new_const(fd: i32, mode: BufferMode, flags: u32) -> FILE {
        FILE {
            fd,
            buf: [0u8; BUFSIZ],
            buf_pos: 0,
            buf_len: 0,
            flags,
            mode,
            ungot: -1,
        }
    }

    /// Construct a FILE at runtime (same layout as `new_const`).
    pub fn new(fd: i32, mode: BufferMode, flags: u32) -> FILE {
        FILE::new_const(fd, mode, flags)
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
}
