use slopos_ostd::KVec;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockDeviceError {
    OutOfBounds,
    InvalidBuffer,
}

/// Stable, enumeration-order identity for a block device. `disk0` is the
/// first device claimed during PCI probe (by convention the root filesystem
/// image); `disk1` the second (a scratch device for destructive tests), etc.
/// Assigned by the driver at probe time and used to look a device up in the
/// driver's registry without naming a raw bus address.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockDeviceIndex(pub u16);

pub trait BlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError>;
    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError>;
    fn capacity(&self) -> u64;
}

pub struct MemoryBlockDevice {
    buffer: slopos_ostd::sync::SpinLock<KVec<u8>>,
}

impl MemoryBlockDevice {
    pub fn allocate(len: usize) -> Option<Self> {
        let mut buffer = KVec::with_capacity(len).ok()?;
        for _ in 0..len {
            buffer.push(0).ok()?;
        }
        Some(Self {
            buffer: slopos_ostd::sync::SpinLock::new(
                buffer,
                slopos_ostd::sync::LOCK_LEVEL_RESOURCE,
            ),
        })
    }

    /// Return a mutable view of the backing buffer for in-place
    /// fixture construction (e.g. test images). Production paths
    /// should use [`BlockDevice::write_at`].
    pub fn with_buffer_mut<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let mut guard = self.buffer.lock();
        f(guard.as_mut_slice())
    }

    pub fn capacity_inner(&self) -> usize {
        self.buffer.lock().len()
    }
}

impl BlockDevice for MemoryBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let guard = self.buffer.lock();
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > guard.len() as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        let start = offset as usize;
        buffer.copy_from_slice(&guard[start..start + buffer.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let mut guard = self.buffer.lock();
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > guard.len() as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        let start = offset as usize;
        guard[start..start + buffer.len()].copy_from_slice(buffer);
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.buffer.lock().len() as u64
    }
}

pub type ReadFn = fn(u64, &mut [u8]) -> bool;
pub type WriteFn = fn(u64, &[u8]) -> bool;
pub type CapacityFn = fn() -> u64;

pub struct CallbackBlockDevice {
    read_fn: ReadFn,
    write_fn: WriteFn,
    capacity_fn: CapacityFn,
}

impl CallbackBlockDevice {
    pub fn new(read_fn: ReadFn, write_fn: WriteFn, capacity_fn: CapacityFn) -> Self {
        Self {
            read_fn,
            write_fn,
            capacity_fn,
        }
    }
}

impl BlockDevice for CallbackBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if (self.read_fn)(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if (self.write_fn)(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn capacity(&self) -> u64 {
        (self.capacity_fn)()
    }
}
