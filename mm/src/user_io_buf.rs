use slopos_abi::Errno;
use slopos_abi::io::{IoBufRead, IoBufWrite};

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use crate::user_ptr::UserBytes;

pub struct UserReadBuf {
    addr: u64,
    len: usize,
}

impl UserReadBuf {
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        if addr == 0 {
            return None;
        }
        addr.checked_add(len as u64)?;
        Some(Self { addr, len })
    }
}

impl IoBufRead for UserReadBuf {
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno> {
        if offset > self.len {
            return Err(Errno::EFAULT);
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
        if addr == 0 {
            return None;
        }
        addr.checked_add(len as u64)?;
        Some(Self { addr, len })
    }
}

impl IoBufWrite for UserWriteBuf {
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno> {
        if offset > self.len {
            return Err(Errno::EFAULT);
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
