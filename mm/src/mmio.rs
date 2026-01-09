//! MMIO region abstraction - type-safe device register access.
//!
//! This module provides the `MmioRegion` type for safe access to memory-mapped
//! I/O regions. It is equivalent to Linux's `ioremap()` + `__iomem` pattern.
//!
//! # Why MMIO Needs Special Handling
//!
//! Memory-mapped I/O regions have special semantics:
//!
//! 1. **Volatile access required** - Reads/writes have side effects and must
//!    not be optimized away or reordered by the compiler.
//!
//! 2. **Alignment requirements** - Device registers often require naturally
//!    aligned access (32-bit registers at 4-byte boundaries, etc.).
//!
//! 3. **No caching** - MMIO regions are typically marked uncacheable in page
//!    tables. The compiler must not assume values persist between accesses.
//!
//! # Usage
//!
//! ```ignore
//! use slopos_abi::addr::PhysAddr;
//! use slopos_mm::mmio::MmioRegion;
//!
//! // Map an MMIO region (e.g., Local APIC at 0xFEE00000)
//! let apic_phys = PhysAddr::new(0xFEE00000);
//! let apic = MmioRegion::map(apic_phys, 0x1000)?;
//!
//! // Read the APIC ID register at offset 0x20
//! let apic_id: u32 = apic.read(0x20);
//!
//! // Write to the EOI register at offset 0xB0
//! apic.write(0xB0, 0u32);
//! ```
//!
//! # Linux Comparison
//!
//! | Linux | SlopOS |
//! |-------|--------|
//! | `ioremap(phys, size)` | `MmioRegion::map(phys, size)` |
//! | `readl(ptr)` | `region.read::<u32>(offset)` |
//! | `writel(val, ptr)` | `region.write::<u32>(offset, val)` |
//! | `iounmap(ptr)` | (drop or explicit unmap) |
//! | `__iomem *` annotation | `MmioRegion` type |

use core::ptr::{read_volatile, write_volatile};

use slopos_abi::addr::PhysAddr;

use crate::hhdm;

/// A mapped MMIO region providing safe volatile access to device registers.
///
/// This type guarantees:
/// - All accesses are volatile (not optimized away)
/// - Bounds checking (debug builds)
/// - Proper alignment verification (debug builds)
///
/// Like Linux's `__iomem *`, this type cannot be dereferenced directly.
/// Use `read()` and `write()` methods for proper volatile access.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MmioRegion {
    virt_base: u64,
    size: usize,
}

impl MmioRegion {
    /// Create an empty (unmapped) MMIO region.
    #[inline]
    pub const fn empty() -> Self {
        Self { virt_base: 0, size: 0 }
    }

    /// Map a physical MMIO region via HHDM.
    ///
    /// Returns `None` if:
    /// - Physical address is null
    /// - Size is zero
    /// - Address + size would overflow
    /// - HHDM is not available
    ///
    /// # Note
    ///
    /// This relies on the HHDM direct mapping provided by the bootloader.
    /// For MMIO regions outside the physical address space covered by HHDM,
    /// explicit page table mappings would be needed (not yet implemented).
    pub fn map(phys: PhysAddr, size: usize) -> Option<Self> {
        if phys.is_null() || size == 0 {
            return None;
        }

        // Check for overflow
        phys.as_u64().checked_add(size as u64)?;

        if !hhdm::is_available() {
            return None;
        }

        Some(Self {
            virt_base: phys.as_u64() + hhdm::offset(),
            size,
        })
    }

    /// Map a single 4KB page at physical address.
    ///
    /// Convenience wrapper for `map(phys, 4096)`.
    pub fn map_page(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, 4096)
    }

    /// Map a 1MB region at physical address.
    ///
    /// Useful for legacy devices with large register spaces.
    pub fn map_1mb(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, 1024 * 1024)
    }

    /// Read a value at byte offset from the MMIO region.
    ///
    /// The type `T` must be `Copy` and should typically be a primitive type
    /// (`u8`, `u16`, `u32`, `u64`).
    ///
    /// # Panics
    ///
    /// - Debug-panics if `offset + sizeof(T)` exceeds region size.
    /// - Debug-panics if access is not naturally aligned.
    #[inline]
    pub fn read<T: Copy>(&self, offset: usize) -> T {
        let size = core::mem::size_of::<T>();
        let end = offset.checked_add(size).expect("offset overflow");

        debug_assert!(
            end <= self.size,
            "MMIO read out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );

        debug_assert!(
            offset % size == 0,
            "MMIO read misaligned: offset={}, align={}",
            offset,
            size
        );

        let ptr = (self.virt_base + offset as u64) as *const T;
        // SAFETY: MmioRegion guarantees valid HHDM mapping, bounds checked above
        unsafe { read_volatile(ptr) }
    }

    /// Write a value at byte offset to the MMIO region.
    ///
    /// The type `T` must be `Copy` and should typically be a primitive type
    /// (`u8`, `u16`, `u32`, `u64`).
    ///
    /// # Panics
    ///
    /// - Debug-panics if `offset + sizeof(T)` exceeds region size.
    /// - Debug-panics if access is not naturally aligned.
    #[inline]
    pub fn write<T: Copy>(&self, offset: usize, value: T) {
        let size = core::mem::size_of::<T>();
        let end = offset.checked_add(size).expect("offset overflow");

        debug_assert!(
            end <= self.size,
            "MMIO write out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );

        debug_assert!(
            offset % size == 0,
            "MMIO write misaligned: offset={}, align={}",
            offset,
            size
        );

        let ptr = (self.virt_base + offset as u64) as *mut T;
        // SAFETY: MmioRegion guarantees valid HHDM mapping, bounds checked above
        unsafe { write_volatile(ptr, value) }
    }

    /// Read a u8 at byte offset (convenience method).
    #[inline]
    pub fn read_u8(&self, offset: usize) -> u8 {
        self.read(offset)
    }

    /// Read a u16 at byte offset (convenience method).
    #[inline]
    pub fn read_u16(&self, offset: usize) -> u16 {
        self.read(offset)
    }

    /// Read a u32 at byte offset (convenience method).
    #[inline]
    pub fn read_u32(&self, offset: usize) -> u32 {
        self.read(offset)
    }

    /// Read a u64 at byte offset (convenience method).
    #[inline]
    pub fn read_u64(&self, offset: usize) -> u64 {
        self.read(offset)
    }

    /// Write a u8 at byte offset (convenience method).
    #[inline]
    pub fn write_u8(&self, offset: usize, value: u8) {
        self.write(offset, value)
    }

    /// Write a u16 at byte offset (convenience method).
    #[inline]
    pub fn write_u16(&self, offset: usize, value: u16) {
        self.write(offset, value)
    }

    /// Write a u32 at byte offset (convenience method).
    #[inline]
    pub fn write_u32(&self, offset: usize, value: u32) {
        self.write(offset, value)
    }

    /// Write a u64 at byte offset (convenience method).
    #[inline]
    pub fn write_u64(&self, offset: usize, value: u64) {
        self.write(offset, value)
    }

    /// Get the virtual base address of this region.
    ///
    /// **Warning**: Do not dereference this directly. Use `read()`/`write()`.
    #[inline]
    pub fn virt_base(&self) -> u64 {
        self.virt_base
    }

    /// Get the physical base address of this region.
    #[inline]
    pub fn phys_base(&self) -> PhysAddr {
        PhysAddr::new(self.virt_base - hhdm::offset())
    }

    /// Get the size of this region in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns true if this region is mapped (non-zero size).
    #[inline]
    pub fn is_mapped(&self) -> bool {
        self.size != 0
    }

    /// Check if an offset is within bounds for a given access size.
    #[inline]
    pub fn is_valid_offset(&self, offset: usize, access_size: usize) -> bool {
        offset.checked_add(access_size).is_some_and(|end| end <= self.size)
    }

    /// Get a sub-region at the specified offset.
    ///
    /// Returns `None` if the sub-region would exceed bounds.
    pub fn sub_region(&self, offset: usize, size: usize) -> Option<MmioRegion> {
        let end = offset.checked_add(size)?;
        if end > self.size {
            return None;
        }
        Some(MmioRegion {
            virt_base: self.virt_base + offset as u64,
            size,
        })
    }
}

impl Default for MmioRegion {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

// MmioRegion is Send but not Sync by default.
// Device registers typically require external synchronization.
// Individual drivers can implement their own locking as needed.

// SAFETY: MmioRegion can be sent between threads - the underlying mapping
// is valid process-wide. However, concurrent access requires synchronization.
unsafe impl Send for MmioRegion {}

// Note: MmioRegion is intentionally NOT Sync. Concurrent MMIO access to the
// same device registers requires explicit synchronization (mutex, etc.).
// Drivers should wrap MmioRegion in appropriate synchronization primitives.
