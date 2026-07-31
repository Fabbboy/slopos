//! Memfd-backed shared memory RAII wrappers.
//!
//! All buffer sharing uses memfd_create + ftruncate + mmap(MAP_SHARED).

use core::ptr::NonNull;

use slopos_abi::MemfdError;
use slopos_abi::syscall::posix::{MAP_SHARED, PROT_READ, PROT_WRITE};

use crate::sys;

/// Owner-side pixel buffer backed by a memfd (anonymous shared memory fd).
pub struct MemfdBuffer {
    fd: i32,
    ptr: NonNull<u8>,
    size: usize,
}

impl MemfdBuffer {
    pub fn create(size: usize) -> Result<Self, MemfdError> {
        if size == 0 {
            return Err(MemfdError::InvalidSize);
        }

        let fd = sys::memfd_create(0);
        if fd < 0 {
            return Err(MemfdError::AllocationFailed);
        }

        let rc = sys::ftruncate(fd, size as u64);
        if rc < 0 {
            sys::close(fd);
            return Err(MemfdError::AllocationFailed);
        }

        let vaddr = sys::mmap(
            0,
            size as u64,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as i64,
            0,
        );
        if vaddr == 0 || (vaddr as i64) < 0 {
            sys::close(fd);
            return Err(MemfdError::MappingFailed);
        }

        let ptr = NonNull::new(vaddr as *mut u8).ok_or_else(|| {
            sys::close(fd);
            MemfdError::MappingFailed
        })?;

        Ok(Self { fd, ptr, size })
    }

    #[inline]
    pub fn fd(&self) -> i32 {
        self.fd
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let (ptr, size) = (self.ptr, self.size);
        slopos_ostd::util::ptr_buf::anchored_nonnull_mut::<_, u8>(self, ptr, size)
    }
}

impl Drop for MemfdBuffer {
    fn drop(&mut self) {
        sys::munmap(self.ptr.as_ptr() as u64, self.size as u64);
        sys::close(self.fd);
    }
}
