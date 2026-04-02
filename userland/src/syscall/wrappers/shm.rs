//! Shared memory RAII wrappers.

use core::num::NonZeroU32;
use core::ptr::NonNull;

use crate::syscall::memory;
use slopos_abi::{SHM_ACCESS_RO, SHM_ACCESS_RW, ShmError};

pub struct ShmBuffer {
    token: NonZeroU32,
    ptr: NonNull<u8>,
    size: usize,
}

impl ShmBuffer {
    pub fn create(size: usize) -> Result<Self, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidSize);
        }

        let token_raw = memory::shm_create(size as u64, 0);
        let token = NonZeroU32::new(token_raw).ok_or(ShmError::AllocationFailed)?;

        let ptr_raw = memory::shm_map(token_raw, SHM_ACCESS_RW);
        if ptr_raw == 0 || (ptr_raw as i64) < 0 {
            memory::shm_destroy(token_raw);
            return Err(ShmError::MappingFailed);
        }

        let ptr = NonNull::new(ptr_raw as *mut u8).ok_or_else(|| {
            memory::shm_destroy(token_raw);
            ShmError::MappingFailed
        })?;

        Ok(Self { token, ptr, size })
    }

    #[inline]
    pub fn token(&self) -> u32 {
        self.token.get()
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.size) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe {
            memory::shm_unmap(self.ptr.as_ptr() as u64);
        }
        memory::shm_destroy(self.token.get());
    }
}

pub struct ShmBufferRef {
    token: NonZeroU32,
    ptr: NonNull<u8>,
    size: usize,
}

impl ShmBufferRef {
    pub fn map_readonly(token: u32, size: usize) -> Result<Self, ShmError> {
        let token_nz = NonZeroU32::new(token).ok_or(ShmError::InvalidToken)?;

        if size == 0 {
            return Err(ShmError::InvalidSize);
        }

        let ptr_raw = memory::shm_map(token, SHM_ACCESS_RO);
        if ptr_raw == 0 || (ptr_raw as i64) < 0 {
            return Err(ShmError::MappingFailed);
        }

        let ptr = NonNull::new(ptr_raw as *mut u8).ok_or(ShmError::MappingFailed)?;

        Ok(Self {
            token: token_nz,
            ptr,
            size,
        })
    }

    #[inline]
    pub fn token(&self) -> u32 {
        self.token.get()
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    #[inline]
    pub fn slice(&self, start: usize, len: usize) -> Option<&[u8]> {
        if start.saturating_add(len) <= self.size {
            Some(&self.as_slice()[start..start + len])
        } else {
            None
        }
    }
}

impl Drop for ShmBufferRef {
    fn drop(&mut self) {
        unsafe {
            memory::shm_unmap(self.ptr.as_ptr() as u64);
        }
    }
}

pub struct CachedShmMapping {
    vaddr: u64,
    size: usize,
}

impl CachedShmMapping {
    pub fn map_readonly(token: u32, size: usize) -> Option<Self> {
        if token == 0 || size == 0 {
            return None;
        }

        let vaddr = memory::shm_map(token, SHM_ACCESS_RO);
        // shm_map returns 0 on some failures and negative errno on others.
        // Any value with the sign bit set is an error.
        if vaddr == 0 || (vaddr as i64) < 0 {
            return None;
        }

        Some(Self { vaddr, size })
    }

    #[inline]
    pub fn vaddr(&self) -> u64 {
        self.vaddr
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vaddr as *const u8, self.size) }
    }

    #[inline]
    pub fn slice(&self, start: usize, len: usize) -> Option<&[u8]> {
        if start.saturating_add(len) <= self.size {
            Some(&self.as_slice()[start..start + len])
        } else {
            None
        }
    }
}
