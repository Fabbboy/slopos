extern crate alloc;

use alloc::vec::Vec;
use slopos_abi::Errno;
use slopos_abi::io::{IoBufRead, IoBufWrite};

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use crate::user_ptr::{UserBytes, UserVirtAddr};

/// Allocate a kernel buffer and copy user data into it in one step.
///
/// Inspired by Linux `memdup_user()`. Returns `ENOMEM` if the
/// allocation fails (never panics), `EFAULT` if the copy fails.
/// Rejects requests larger than `max_size` bytes with `EINVAL`.
pub fn memdup_user(addr: u64, len: usize, max_size: usize) -> Result<Vec<u8>, Errno> {
    if len > max_size {
        return Err(Errno::EINVAL);
    }
    let user_bytes = UserBytes::try_new(addr, len).map_err(|_| Errno::EFAULT)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    buf.resize(len, 0);
    copy_bytes_from_user(user_bytes, &mut buf).map_err(|_| Errno::EFAULT)?;
    Ok(buf)
}

/// Validates that `[addr, addr+len)` lies entirely within user-space.
///
/// This is the upfront `access_ok()` equivalent — rejects null,
/// non-canonical, kernel-space, and overflowing ranges before any
/// I/O buffer is constructed.  Individual copy operations re-validate
/// via `UserBytes::try_new` for defense-in-depth.
fn validate_user_range(addr: u64, len: usize) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    UserVirtAddr::try_new(addr, len).map_err(|_| Errno::EFAULT)?;
    Ok(())
}

pub struct UserReadBuf {
    addr: u64,
    len: usize,
}

impl UserReadBuf {
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        validate_user_range(addr, len).ok()?;
        Some(Self { addr, len })
    }
}

impl IoBufRead for UserReadBuf {
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno> {
        if offset >= self.len {
            return Ok(0);
        }
        let remaining = self.len - offset;
        let n = dst.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let source_addr = self.addr.checked_add(offset as u64).ok_or(Errno::EFAULT)?;
        let user_bytes = UserBytes::try_new(source_addr, n).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_bytes, &mut dst[..n]).map_err(|_| Errno::EFAULT)
    }

    fn len(&self) -> usize {
        self.len
    }
}

pub struct UserWriteBuf {
    addr: u64,
    len: usize,
}

impl UserWriteBuf {
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        validate_user_range(addr, len).ok()?;
        Some(Self { addr, len })
    }
}

impl IoBufWrite for UserWriteBuf {
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno> {
        if offset >= self.len {
            return Ok(0);
        }
        let remaining = self.len - offset;
        let n = src.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let target_addr = self.addr.checked_add(offset as u64).ok_or(Errno::EFAULT)?;
        let user_bytes = UserBytes::try_new(target_addr, n).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_bytes, &src[..n]).map_err(|_| Errno::EFAULT)
    }

    fn len(&self) -> usize {
        self.len
    }
}
