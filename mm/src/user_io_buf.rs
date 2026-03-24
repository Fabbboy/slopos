//! [`IoBuf`] implementation for user-space memory regions.
//!
//! [`UserIoBuf`] wraps a user-space virtual address and length,
//! performing validated `copy_to_user` / `copy_from_user` transfers
//! on every `write_at` / `read_at` call.  This is the kernel-side
//! counterpart of a user-land `&mut [u8]` buffer passed through a
//! syscall — it lets VFS, ext2, pipes, and other subsystems write
//! data directly into user memory without an intermediate bounce
//! buffer.

use slopos_abi::io::IoBuf;

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use crate::user_ptr::UserBytes;

/// An [`IoBuf`] backed by a validated user-space address range.
///
/// Created by syscall handlers when dispatching `read()` / `write()`
/// calls, so the file-system layer can transfer data directly
/// to/from user memory.
pub struct UserIoBuf {
    addr: u64,
    len: usize,
}

impl UserIoBuf {
    /// Create a new user I/O buffer.
    ///
    /// The caller must ensure `addr` is a valid user-space address
    /// (the actual page-level validation happens lazily in
    /// `copy_bytes_to_user` / `copy_bytes_from_user`).
    ///
    /// Returns `None` if `addr` is zero.
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        if addr == 0 {
            return None;
        }
        Some(Self { addr, len })
    }
}

impl IoBuf for UserIoBuf {
    fn write_at(&mut self, offset: usize, src: &[u8]) -> Result<usize, i32> {
        if offset >= self.len {
            return Err(-14); // EFAULT
        }
        let remaining = self.len - offset;
        let n = src.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let target_addr = self.addr.checked_add(offset as u64).ok_or(-14i32)?;
        let user_bytes = UserBytes::try_new(target_addr, n).map_err(|_| -14i32)?;
        copy_bytes_to_user(user_bytes, &src[..n]).map_err(|_| -14i32)
    }

    fn read_at(&self, offset: usize, dst: &mut [u8]) -> Result<usize, i32> {
        if offset >= self.len {
            return Err(-14); // EFAULT
        }
        let remaining = self.len - offset;
        let n = dst.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let source_addr = self.addr.checked_add(offset as u64).ok_or(-14i32)?;
        let user_bytes = UserBytes::try_new(source_addr, n).map_err(|_| -14i32)?;
        copy_bytes_from_user(user_bytes, &mut dst[..n]).map_err(|_| -14i32)
    }

    fn len(&self) -> usize {
        self.len
    }
}
