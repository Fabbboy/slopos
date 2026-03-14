//! Generic intrusive doubly-linked free-list allocator.
//!
//! # Memory Layout
//!
//! ```text
//! +----------------+
//! | magic (4)      |  <- BlockHeader starts here
//! | size (4)       |
//! | flags (4)      |
//! | checksum (4)   |
//! | next (8)       |
//! | prev (8)       |
//! +----------------+
//! | user data...   |  <- Pointer returned to caller
//! +----------------+
//! ```

use core::ptr;

pub const MAGIC_FREE: u32 = 0xFEED_FACE;
pub const MAGIC_ALLOCATED: u32 = 0xDEAD_BEEF;
pub const MIN_BLOCK_SIZE: usize = 16;
pub const DEFAULT_ALIGNMENT: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockHeader {
    pub magic: u32,
    pub size: u32,
    pub flags: u32,
    pub checksum: u32,
    pub next: *mut BlockHeader,
    pub prev: *mut BlockHeader,
}

pub const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

impl BlockHeader {
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

    /// # Safety
    /// `block` must point to valid, writable, properly aligned memory.
    #[inline]
    pub unsafe fn init(block: *mut BlockHeader, size: u32, magic: u32) {
        debug_assert!(!block.is_null());
        unsafe {
            let header = &mut *block;
            header.magic = magic;
            header.size = size;
            header.flags = 0;
            header.checksum = Self::compute_checksum(magic, size, 0);
            header.next = ptr::null_mut();
            header.prev = ptr::null_mut();
        }
    }

    #[inline]
    pub const fn compute_checksum(magic: u32, size: u32, flags: u32) -> u32 {
        magic ^ size ^ flags
    }

    #[inline]
    pub fn update_checksum(&mut self) {
        self.checksum = Self::compute_checksum(self.magic, self.size, self.flags);
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        if self.magic != MAGIC_FREE && self.magic != MAGIC_ALLOCATED {
            return false;
        }
        self.checksum == Self::compute_checksum(self.magic, self.size, self.flags)
    }

    #[inline]
    pub fn is_free(&self) -> bool {
        self.magic == MAGIC_FREE
    }

    #[inline]
    pub fn is_allocated(&self) -> bool {
        self.magic == MAGIC_ALLOCATED
    }

    #[inline]
    pub fn mark_free(&mut self) {
        self.magic = MAGIC_FREE;
        self.update_checksum();
    }

    #[inline]
    pub fn mark_allocated(&mut self) {
        self.magic = MAGIC_ALLOCATED;
        self.update_checksum();
    }

    /// # Safety
    /// `block` must point to a valid block.
    #[inline]
    pub unsafe fn data_ptr(block: *mut BlockHeader) -> *mut u8 {
        unsafe { (block as *mut u8).add(HEADER_SIZE) }
    }

    /// # Safety
    /// `data` must have been returned by a previous call to `data_ptr`.
    #[inline]
    pub unsafe fn from_data_ptr(data: *mut u8) -> *mut BlockHeader {
        unsafe { data.sub(HEADER_SIZE) as *mut BlockHeader }
    }

    #[inline]
    pub const fn total_size(&self) -> usize {
        HEADER_SIZE + self.size as usize
    }

    /// # Safety
    /// Only valid if this block is part of a contiguous memory region.
    #[inline]
    pub unsafe fn block_end(block: *mut BlockHeader) -> *mut u8 {
        unsafe {
            let header = &*block;
            (block as *mut u8).add(header.total_size())
        }
    }
}

#[derive(Clone, Copy)]
pub struct FreeList {
    pub head: *mut BlockHeader,
    pub count: u32,
}

impl FreeList {
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            count: 0,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    /// # Safety
    /// `block` must point to a valid `BlockHeader` not already in any free list.
    pub unsafe fn push_front(&mut self, block: *mut BlockHeader) {
        debug_assert!(!block.is_null());
        unsafe {
            let header = &mut *block;
            header.prev = ptr::null_mut();
            header.next = self.head;
            if !self.head.is_null() {
                (*self.head).prev = block;
            }
            self.head = block;
            self.count += 1;
        }
    }

    /// # Safety
    /// `block` must be in this free list.
    pub unsafe fn remove(&mut self, block: *mut BlockHeader) {
        debug_assert!(!block.is_null());
        unsafe {
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
    }

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

    /// # Safety
    /// The callback must not modify the list structure.
    pub unsafe fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(*mut BlockHeader),
    {
        let mut current = self.head;
        unsafe {
            while !current.is_null() {
                let next = (*current).next;
                f(current);
                current = next;
            }
        }
    }
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}

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

/// # Safety
/// `block` must point to a valid `BlockHeader` removed from any free list.
pub unsafe fn try_split_block(
    block: *mut BlockHeader,
    requested_size: usize,
    min_split_size: usize,
) -> *mut BlockHeader {
    debug_assert!(!block.is_null());
    unsafe {
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
}

/// # Safety
/// `block` must point to a valid free `BlockHeader`.
pub unsafe fn try_coalesce<F>(block: *mut BlockHeader, get_next_physical: F) -> bool
where
    F: FnOnce(*mut BlockHeader) -> *mut BlockHeader,
{
    debug_assert!(!block.is_null());
    let next_physical = get_next_physical(block);
    if next_physical.is_null() {
        return false;
    }
    unsafe {
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
}
