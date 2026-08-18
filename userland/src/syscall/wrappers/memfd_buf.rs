//! Shared memory RAII wrappers (memfd-backed, fd-only).

use core::ptr::NonNull;

use crate::syscall::memory;
use slopos_abi::MemfdError;
use slopos_abi::syscall::posix::{MAP_SHARED, PROT_READ, PROT_WRITE};

/// Owner-side shared memory buffer backed by a memfd.
pub struct ShmBuffer {
    fd: i32,
    ptr: NonNull<u8>,
    size: usize,
}

impl ShmBuffer {
    pub fn create(size: usize) -> Result<Self, MemfdError> {
        if size == 0 {
            return Err(MemfdError::InvalidSize);
        }

        let fd = memory::memfd_create(0);
        if fd < 0 {
            return Err(MemfdError::AllocationFailed);
        }

        let rc = memory::ftruncate(fd, size as u64);
        if rc < 0 {
            memory::close(fd);
            return Err(MemfdError::AllocationFailed);
        }

        let vaddr = memory::mmap(
            0,
            size as u64,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as i64,
            0,
        );
        if vaddr == 0 || (vaddr as i64) < 0 {
            memory::close(fd);
            return Err(MemfdError::MappingFailed);
        }

        let ptr = NonNull::new(vaddr as *mut u8).ok_or_else(|| {
            memory::close(fd);
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
        memory::munmap(self.ptr.as_ptr() as u64, self.size as u64);
        memory::close(self.fd);
    }
}

/// Read-only mapping of a memfd received via SCM_RIGHTS (compositor side).
///
/// `owns_fd` decides whether drop closes the fd as well as unmapping. A
/// `MAP_SHARED` mapping stays valid after its fd is closed, so a borrowed
/// mapping outliving the fd is safe.
pub struct CachedShmMapping {
    fd: i32,
    vaddr: u64,
    size: usize,
    owns_fd: bool,
}

impl CachedShmMapping {
    /// Takes ownership of `fd`; drop closes it.
    pub fn map_readonly_fd(fd: i32, size: usize) -> Option<Self> {
        Self::map_readonly_inner(fd, size, true)
    }

    /// Borrows `fd`; drop releases only the mapping.
    pub fn map_readonly_fd_borrowed(fd: i32, size: usize) -> Option<Self> {
        Self::map_readonly_inner(fd, size, false)
    }

    fn map_readonly_inner(fd: i32, size: usize, owns_fd: bool) -> Option<Self> {
        if fd < 0 || size == 0 {
            return None;
        }

        let vaddr = memory::mmap(0, size as u64, PROT_READ, MAP_SHARED, fd as i64, 0);
        if vaddr == 0 || (vaddr as i64) < 0 {
            return None;
        }

        Some(Self {
            fd,
            vaddr,
            size,
            owns_fd,
        })
    }

    /// Read/write mapping; takes ownership of `fd`.
    pub fn map_writable_fd(fd: i32, size: usize) -> Option<Self> {
        if fd < 0 || size == 0 {
            return None;
        }

        let vaddr = memory::mmap(
            0,
            size as u64,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as i64,
            0,
        );
        if vaddr == 0 || (vaddr as i64) < 0 {
            return None;
        }

        Some(Self {
            fd,
            vaddr,
            size,
            owns_fd: true,
        })
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vaddr as *mut u8, self.size) }
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

impl Drop for CachedShmMapping {
    fn drop(&mut self) {
        memory::munmap(self.vaddr, self.size as u64);
        if self.owns_fd {
            memory::close(self.fd);
        }
    }
}
