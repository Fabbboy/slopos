use slopos_abi::Errno;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_ostd::KVec;

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use crate::user_ptr::{UserBytes, UserVirtAddr};

/// Allocate a kernel buffer and copy user data into it in one step.
///
/// `EINVAL` above `max_size`, `ENOMEM` if the allocation fails (never
/// panics), `EFAULT` if the copy fails.
pub fn memdup_user(addr: u64, len: usize, max_size: usize) -> Result<KVec<u8>, Errno> {
    if len > max_size {
        return Err(Errno::EINVAL);
    }
    let user_bytes = UserBytes::try_new(addr, len).map_err(|_| Errno::EFAULT)?;
    let mut buf = KVec::<u8>::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    copy_bytes_from_user(user_bytes, &mut buf).map_err(|_| Errno::EFAULT)?;
    Ok(buf)
}

/// The upfront `access_ok()` equivalent: rejects null, non-canonical,
/// kernel-space and overflowing ranges before any I/O buffer is constructed.
/// Individual copies still re-validate via `UserBytes::try_new`.
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
