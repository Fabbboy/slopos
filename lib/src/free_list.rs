//! Generic intrusive doubly-linked free-list allocator.
//!
//! This module provides a reusable free-list implementation that can be used
//! by both kernel heap and userland allocators. The design uses intrusive
//! block headers embedded at the start of each allocation.
//!
//! # Features
//!
//! - Doubly-linked free list for O(1) insertion/removal
//! - Magic number validation for corruption detection
//! - Optional checksum validation (configurable)
//! - Block splitting on allocation
//! - Block coalescing on free (optional)
//! - First-fit allocation strategy
//!
//! # Usage
//!
//! ```ignore
//! use slopos_lib::free_list::{FreeList, BlockHeader, MAGIC_FREE, MAGIC_ALLOCATED};
//!
//! // Create a new free list
//! let mut free_list = FreeList::new();
//!
//! // Initialize a block and add it to the free list
//! let block: *mut BlockHeader = /* ... */;
//! unsafe {
//!     BlockHeader::init(block, size, MAGIC_FREE);
//!     free_list.push_front(block);
//! }
//!
//! // Find and remove a block
//! if let Some(block) = free_list.find_first_fit(needed_size) {
//!     free_list.remove(block);
//!     // Use the block...
//! }
//! ```

use core::ptr;

/// Magic number indicating a free block.
pub const MAGIC_FREE: u32 = 0xFEED_FACE;

/// Magic number indicating an allocated block.
pub const MAGIC_ALLOCATED: u32 = 0xDEAD_BEEF;

/// Minimum allocation size (must be at least large enough for the free list pointers).
pub const MIN_BLOCK_SIZE: usize = 16;

/// Default alignment for allocations.
pub const DEFAULT_ALIGNMENT: usize = 16;

/// Block header for intrusive free-list allocator.
///
/// This header is embedded at the start of each memory block. When the block
/// is free, the `next` and `prev` pointers form a doubly-linked list. When
/// allocated, these pointers are unused but preserved for validation.
///
/// # Memory Layout
///
/// ```text
/// +----------------+
/// | magic (4)      |  <- BlockHeader starts here
/// | size (4)       |
/// | flags (4)      |
/// | checksum (4)   |
/// | next (8)       |
/// | prev (8)       |
/// +----------------+
/// | user data...   |  <- Pointer returned to caller
/// +----------------+
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockHeader {
    /// Magic number for validation (MAGIC_FREE or MAGIC_ALLOCATED).
    pub magic: u32,
    /// Size of the user data portion (excludes header).
    pub size: u32,
    /// Flags (reserved for future use, e.g., alignment requirements).
    pub flags: u32,
    /// XOR checksum of magic, size, and flags for corruption detection.
    pub checksum: u32,
    /// Next block in the free list (null if last or allocated).
    pub next: *mut BlockHeader,
    /// Previous block in the free list (null if first or allocated).
    pub prev: *mut BlockHeader,
}

/// Size of the block header in bytes.
pub const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

impl BlockHeader {
    /// Create an empty/null block header.
    pub const fn empty() -> Self {
        Self {
            magic: 0,
            size: 0,
            flags: 0,
            checksum: 0,
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        }
    }

    /// Initialize a block header at the given address.
    ///
    /// # Safety
    ///
    /// - `block` must point to valid, writable memory of at least `HEADER_SIZE` bytes.
    /// - The memory region must be properly aligned for `BlockHeader`.
    #[inline]
    pub unsafe fn init(block: *mut BlockHeader, size: u32, magic: u32) {
        debug_assert!(!block.is_null());
        let header = &mut *block;
        header.magic = magic;
        header.size = size;
        header.flags = 0;
        header.checksum = Self::compute_checksum(magic, size, 0);
        header.next = ptr::null_mut();
        header.prev = ptr::null_mut();
    }

    /// Compute the checksum for validation.
    #[inline]
    pub const fn compute_checksum(magic: u32, size: u32, flags: u32) -> u32 {
        magic ^ size ^ flags
    }

    /// Update the checksum after modifying fields.
    #[inline]
    pub fn update_checksum(&mut self) {
        self.checksum = Self::compute_checksum(self.magic, self.size, self.flags);
    }

    /// Validate the block header.
    ///
    /// Returns `true` if:
    /// - Magic is either MAGIC_FREE or MAGIC_ALLOCATED
    /// - Checksum matches the computed value
    #[inline]
    pub fn is_valid(&self) -> bool {
        if self.magic != MAGIC_FREE && self.magic != MAGIC_ALLOCATED {
            return false;
        }
        self.checksum == Self::compute_checksum(self.magic, self.size, self.flags)
    }

    /// Check if this block is free.
    #[inline]
    pub fn is_free(&self) -> bool {
        self.magic == MAGIC_FREE
    }

    /// Check if this block is allocated.
    #[inline]
    pub fn is_allocated(&self) -> bool {
        self.magic == MAGIC_ALLOCATED
    }

    /// Mark this block as free.
    #[inline]
    pub fn mark_free(&mut self) {
        self.magic = MAGIC_FREE;
        self.update_checksum();
    }

    /// Mark this block as allocated.
    #[inline]
    pub fn mark_allocated(&mut self) {
        self.magic = MAGIC_ALLOCATED;
        self.update_checksum();
    }

    /// Get a pointer to the user data portion of this block.
    ///
    /// # Safety
    ///
    /// The returned pointer is only valid if `self` points to a valid block.
    #[inline]
    pub unsafe fn data_ptr(block: *mut BlockHeader) -> *mut u8 {
        (block as *mut u8).add(HEADER_SIZE)
    }

    /// Get the block header from a user data pointer.
    ///
    /// # Safety
    ///
    /// `data` must have been returned by a previous call to `data_ptr`.
    #[inline]
    pub unsafe fn from_data_ptr(data: *mut u8) -> *mut BlockHeader {
        data.sub(HEADER_SIZE) as *mut BlockHeader
    }

    /// Calculate the total block size (header + user data).
    #[inline]
    pub const fn total_size(&self) -> usize {
        HEADER_SIZE + self.size as usize
    }

    /// Get a pointer to immediately after this block (potential next block location).
    ///
    /// # Safety
    ///
    /// Only valid if this block is part of a contiguous memory region.
    #[inline]
    pub unsafe fn block_end(block: *mut BlockHeader) -> *mut u8 {
        let header = &*block;
        (block as *mut u8).add(header.total_size())
    }
}

/// A simple doubly-linked free list.
///
/// This structure manages a linked list of free blocks. It does not own
/// the memory; blocks must be managed externally.
#[derive(Clone, Copy)]
pub struct FreeList {
    /// Head of the free list (most recently freed block).
    pub head: *mut BlockHeader,
    /// Number of blocks in the list.
    pub count: u32,
}

impl FreeList {
    /// Create a new empty free list.
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            count: 0,
        }
    }

    /// Check if the list is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    /// Add a block to the front of the free list.
    ///
    /// # Safety
    ///
    /// - `block` must point to a valid `BlockHeader`.
    /// - The block must not already be in any free list.
    /// - The block's magic should be `MAGIC_FREE`.
    pub unsafe fn push_front(&mut self, block: *mut BlockHeader) {
        debug_assert!(!block.is_null());
        let header = &mut *block;

        header.prev = ptr::null_mut();
        header.next = self.head;

        if !self.head.is_null() {
            (*self.head).prev = block;
        }

        self.head = block;
        self.count += 1;
    }

    /// Remove a specific block from the free list.
    ///
    /// # Safety
    ///
    /// - `block` must be in this free list.
    /// - `block` must point to a valid `BlockHeader`.
    pub unsafe fn remove(&mut self, block: *mut BlockHeader) {
        debug_assert!(!block.is_null());
        let header = &mut *block;

        if !header.prev.is_null() {
            (*header.prev).next = header.next;
        } else {
            self.head = header.next;
        }

        if !header.next.is_null() {
            (*header.next).prev = header.prev;
        }

        header.next = ptr::null_mut();
        header.prev = ptr::null_mut();
        self.count = self.count.saturating_sub(1);
    }

    /// Find the first block that can satisfy an allocation of `min_size` bytes.
    ///
    /// Uses first-fit strategy: returns the first block with size >= min_size.
    ///
    /// # Returns
    ///
    /// Pointer to the first suitable block, or null if none found.
    pub fn find_first_fit(&self, min_size: usize) -> *mut BlockHeader {
        let mut current = self.head;

        while !current.is_null() {
            let header = unsafe { &*current };
            if header.size as usize >= min_size {
                return current;
            }
            current = header.next;
        }

        ptr::null_mut()
    }

    /// Iterate over all blocks in the free list.
    ///
    /// # Safety
    ///
    /// The callback must not modify the list structure.
    pub unsafe fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(*mut BlockHeader),
    {
        let mut current = self.head;
        while !current.is_null() {
            let next = (*current).next;
            f(current);
            current = next;
        }
    }
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}

/// Round up a size to the nearest power of two, with a minimum.
///
/// # Arguments
///
/// * `size` - The size to round up
/// * `min_size` - The minimum size to return
///
/// # Returns
///
/// The smallest power of two >= max(size, min_size)
#[inline]
pub const fn round_up_pow2(size: usize, min_size: usize) -> usize {
    let size = if size < min_size { min_size } else { size };

    if size == 0 {
        return min_size;
    }

    if size & (size - 1) == 0 {
        return size;
    }

    let mut result = 1usize;
    while result < size {
        result <<= 1;
    }
    result
}

/// Calculate the size class index for a given size.
///
/// This implements a logarithmic size class scheme:
/// - Class 0: <= 16 bytes
/// - Class 1: <= 32 bytes
/// - Class 2: <= 64 bytes
/// - ...
/// - Class N: <= 2^(N+4) bytes
///
/// # Arguments
///
/// * `size` - The allocation size
/// * `num_classes` - Total number of size classes
///
/// # Returns
///
/// The size class index (0 to num_classes-1)
#[inline]
pub const fn size_class(size: usize, num_classes: usize) -> usize {
    if size <= 16 {
        return 0;
    }

    let mut class = 0usize;
    let mut threshold = 16usize;

    while class < num_classes - 1 && size > threshold {
        class += 1;
        threshold <<= 1;
    }

    if class >= num_classes {
        num_classes - 1
    } else {
        class
    }
}

/// Split a block if it's large enough to hold the requested size plus a new block.
///
/// # Arguments
///
/// * `block` - The block to potentially split
/// * `requested_size` - The size needed for the allocation (user data only)
/// * `min_split_size` - Minimum remaining size to create a new free block
///
/// # Returns
///
/// If the block was split, returns a pointer to the new free block.
/// Otherwise returns null.
///
/// # Safety
///
/// - `block` must point to a valid `BlockHeader`.
/// - `block` must have been removed from any free list before calling.
pub unsafe fn try_split_block(
    block: *mut BlockHeader,
    requested_size: usize,
    min_split_size: usize,
) -> *mut BlockHeader {
    debug_assert!(!block.is_null());
    let header = &mut *block;

    let min_remainder = min_split_size + HEADER_SIZE;

    if (header.size as usize) < requested_size + min_remainder {
        return ptr::null_mut();
    }

    let new_block_addr = (block as *mut u8).add(HEADER_SIZE + requested_size);
    let new_block = new_block_addr as *mut BlockHeader;
    let new_size = header.size as usize - requested_size - HEADER_SIZE;

    BlockHeader::init(new_block, new_size as u32, MAGIC_FREE);

    header.size = requested_size as u32;
    header.update_checksum();

    new_block
}

/// Attempt to coalesce a block with its physically adjacent successor.
///
/// # Arguments
///
/// * `block` - The block to coalesce
/// * `get_next_physical` - Function to get the next physical block (if any)
///
/// # Returns
///
/// `true` if coalescing occurred, `false` otherwise.
///
/// # Safety
///
/// - `block` must point to a valid free `BlockHeader`.
/// - Both blocks must be in the same free list and must be removed before coalescing.
/// - `get_next_physical` must return a valid block or null.
pub unsafe fn try_coalesce<F>(block: *mut BlockHeader, get_next_physical: F) -> bool
where
    F: FnOnce(*mut BlockHeader) -> *mut BlockHeader,
{
    debug_assert!(!block.is_null());

    let next_physical = get_next_physical(block);
    if next_physical.is_null() {
        return false;
    }

    let header = &mut *block;
    let next_header = &*next_physical;

    if !next_header.is_free() || !next_header.is_valid() {
        return false;
    }

    let expected_next = BlockHeader::block_end(block);
    if next_physical as *mut u8 != expected_next {
        return false;
    }

    header.size += HEADER_SIZE as u32 + next_header.size;
    header.update_checksum();

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_up_pow2() {
        assert_eq!(round_up_pow2(0, 16), 16);
        assert_eq!(round_up_pow2(1, 16), 16);
        assert_eq!(round_up_pow2(16, 16), 16);
        assert_eq!(round_up_pow2(17, 16), 32);
        assert_eq!(round_up_pow2(32, 16), 32);
        assert_eq!(round_up_pow2(33, 16), 64);
        assert_eq!(round_up_pow2(1024, 16), 1024);
        assert_eq!(round_up_pow2(1025, 16), 2048);
    }

    #[test]
    fn test_size_class() {
        assert_eq!(size_class(1, 16), 0);
        assert_eq!(size_class(16, 16), 0);
        assert_eq!(size_class(17, 16), 1);
        assert_eq!(size_class(32, 16), 1);
        assert_eq!(size_class(33, 16), 2);
        assert_eq!(size_class(64, 16), 2);
        assert_eq!(size_class(65, 16), 3);
        assert_eq!(size_class(1_000_000, 16), 15);
    }

    #[test]
    fn test_checksum() {
        let checksum = BlockHeader::compute_checksum(MAGIC_FREE, 1024, 0);
        assert_eq!(checksum, MAGIC_FREE ^ 1024 ^ 0);
    }
}
