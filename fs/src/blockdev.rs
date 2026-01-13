use core::ptr;

use slopos_mm::kernel_heap::{kfree, kmalloc};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockDeviceError {
    OutOfBounds,
    InvalidBuffer,
}

pub trait BlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError>;
    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError>;
    fn capacity(&self) -> u64;
}

pub struct MemoryBlockDevice {
    base: *mut u8,
    len: usize,
    owns_allocation: bool,
}

unsafe impl Send for MemoryBlockDevice {}

impl MemoryBlockDevice {
    pub fn new(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            owns_allocation: false,
        }
    }

    pub fn allocate(len: usize) -> Option<Self> {
        let ptr = kmalloc(len);
        if ptr.is_null() {
            return None;
        }
        unsafe {
            ptr::write_bytes(ptr, 0, len);
        }
        Some(Self {
            base: ptr as *mut u8,
            len,
            owns_allocation: true,
        })
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.base
    }
}

impl Drop for MemoryBlockDevice {
    fn drop(&mut self) {
        if self.owns_allocation && !self.base.is_null() {
            kfree(self.base as *mut _);
            self.base = ptr::null_mut();
            self.len = 0;
        }
    }
}

impl BlockDevice for MemoryBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > self.len as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        if self.base.is_null() {
            return Err(BlockDeviceError::InvalidBuffer);
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self.base.add(offset as usize),
                buffer.as_mut_ptr(),
                buffer.len(),
            );
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > self.len as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        if self.base.is_null() {
            return Err(BlockDeviceError::InvalidBuffer);
        }
        unsafe {
            ptr::copy_nonoverlapping(
                buffer.as_ptr(),
                self.base.add(offset as usize),
                buffer.len(),
            );
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.len as u64
    }
}
