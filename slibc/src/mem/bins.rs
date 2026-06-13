use core::ptr;

use super::chunk::{self, ChunkPtr, MIN_CHUNK_SIZE};

pub const SMALL_BIN_COUNT: usize = 16;
pub const LARGE_BIN_COUNT: usize = 8;
pub const BIN_COUNT: usize = SMALL_BIN_COUNT + LARGE_BIN_COUNT;

const SMALL_BIN_MAX_SIZE: usize = MIN_CHUNK_SIZE + ((SMALL_BIN_COUNT - 1) * chunk::ALIGNMENT);

#[derive(Clone, Copy)]
struct Bin {
    head: ChunkPtr,
}

impl Bin {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
        }
    }
}

pub struct BinArray {
    bins: [Bin; BIN_COUNT],
    unsorted: Bin,
    pub binmap: u32,
}

impl BinArray {
    pub const fn new() -> Self {
        Self {
            bins: [Bin::new(); BIN_COUNT],
            unsorted: Bin::new(),
            binmap: 0,
        }
    }

    #[inline]
    pub fn is_empty(&self, bin_idx: usize) -> bool {
        self.bins[bin_idx].head.is_null()
    }

    #[inline]
    pub fn unsorted_is_empty(&self) -> bool {
        self.unsorted.head.is_null()
    }

    #[inline]
    pub fn first_nonempty_from(&self, start: usize) -> Option<usize> {
        if start >= BIN_COUNT {
            return None;
        }

        let mask = self.binmap & (!0u32 << start);
        if mask == 0 {
            None
        } else {
            Some(mask.trailing_zeros() as usize)
        }
    }

    pub unsafe fn insert(&mut self, bin_idx: usize, chunk_ptr: ChunkPtr) {
        if bin_idx < SMALL_BIN_COUNT {
            unsafe {
                Self::insert_front(&mut self.bins[bin_idx].head, chunk_ptr);
            }
        } else {
            unsafe {
                Self::insert_sorted(&mut self.bins[bin_idx].head, chunk_ptr);
            }
        }
        self.binmap |= 1u32 << bin_idx;
    }

    pub unsafe fn insert_unsorted(&mut self, chunk_ptr: ChunkPtr) {
        unsafe {
            Self::insert_front(&mut self.unsorted.head, chunk_ptr);
        }
    }

    pub unsafe fn pop_front(&mut self, bin_idx: usize) -> ChunkPtr {
        let head = self.bins[bin_idx].head;
        if head.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            self.remove(head);
        }
        head
    }

    pub unsafe fn pop_unsorted_front(&mut self) -> ChunkPtr {
        let head = self.unsorted.head;
        if head.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            self.remove(head);
        }
        head
    }

    pub unsafe fn find_best_fit(&self, bin_idx: usize, request_size: usize) -> ChunkPtr {
        let head = self.bins[bin_idx].head;
        if head.is_null() {
            return ptr::null_mut();
        }

        let mut current = head;
        loop {
            if unsafe { chunk::size(current) } >= request_size {
                return current;
            }

            current = unsafe { chunk::fd(current) };
            if current == head {
                break;
            }
        }

        ptr::null_mut()
    }

    pub unsafe fn remove(&mut self, chunk_ptr: ChunkPtr) {
        if chunk_ptr.is_null() {
            return;
        }

        let next = unsafe { chunk::fd(chunk_ptr) };
        let prev = unsafe { chunk::bk(chunk_ptr) };
        if next.is_null() || prev.is_null() {
            return;
        }

        let mut regular_head = None;
        for idx in 0..BIN_COUNT {
            if self.bins[idx].head == chunk_ptr {
                regular_head = Some(idx);
                break;
            }
        }
        let unsorted_head = self.unsorted.head == chunk_ptr;

        if next == chunk_ptr && prev == chunk_ptr {
            if let Some(idx) = regular_head {
                self.bins[idx].head = ptr::null_mut();
                self.binmap &= !(1u32 << idx);
            } else if unsorted_head {
                self.unsorted.head = ptr::null_mut();
            }
        } else {
            unsafe {
                chunk::set_fd(prev, next);
                chunk::set_bk(next, prev);
            }

            if let Some(idx) = regular_head {
                self.bins[idx].head = next;
            } else if unsorted_head {
                self.unsorted.head = next;
            }
        }

        unsafe {
            chunk::clear_links(chunk_ptr);
        }
    }

    unsafe fn insert_front(head: &mut ChunkPtr, chunk_ptr: ChunkPtr) {
        if head.is_null() {
            unsafe {
                chunk::set_fd(chunk_ptr, chunk_ptr);
                chunk::set_bk(chunk_ptr, chunk_ptr);
            }
            *head = chunk_ptr;
            return;
        }

        unsafe {
            Self::insert_before(head, *head, chunk_ptr);
        }
        *head = chunk_ptr;
    }

    unsafe fn insert_sorted(head: &mut ChunkPtr, chunk_ptr: ChunkPtr) {
        if head.is_null() {
            unsafe {
                chunk::set_fd(chunk_ptr, chunk_ptr);
                chunk::set_bk(chunk_ptr, chunk_ptr);
            }
            *head = chunk_ptr;
            return;
        }

        let new_size = unsafe { chunk::size(chunk_ptr) };
        let mut current = *head;
        loop {
            if unsafe { chunk::size(current) } >= new_size {
                unsafe {
                    Self::insert_before(head, current, chunk_ptr);
                }
                if current == *head {
                    *head = chunk_ptr;
                }
                return;
            }

            current = unsafe { chunk::fd(current) };
            if current == *head {
                break;
            }
        }

        unsafe {
            Self::insert_before(head, *head, chunk_ptr);
        }
    }

    unsafe fn insert_before(_head: &mut ChunkPtr, position: ChunkPtr, chunk_ptr: ChunkPtr) {
        let prev = unsafe { chunk::bk(position) };
        unsafe {
            chunk::set_fd(chunk_ptr, position);
            chunk::set_bk(chunk_ptr, prev);
            chunk::set_fd(prev, chunk_ptr);
            chunk::set_bk(position, chunk_ptr);
        }
    }

    /// Size of the largest chunk across all bins and the unsorted list.
    pub unsafe fn largest_chunk_size(&self) -> usize {
        let mut largest = 0;
        let heads = self
            .bins
            .iter()
            .map(|bin| bin.head)
            .chain(core::iter::once(self.unsorted.head));
        for head in heads {
            if head.is_null() {
                continue;
            }
            let mut current = head;
            loop {
                largest = largest.max(unsafe { chunk::size(current) });
                current = unsafe { chunk::fd(current) };
                if current == head {
                    break;
                }
            }
        }
        largest
    }
}

impl Default for BinArray {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub fn size_to_small_bin(size: usize) -> Option<usize> {
    if !(MIN_CHUNK_SIZE..=SMALL_BIN_MAX_SIZE).contains(&size) {
        return None;
    }

    if size & (chunk::ALIGNMENT - 1) != 0 {
        return None;
    }

    Some((size - MIN_CHUNK_SIZE) / chunk::ALIGNMENT)
}

#[inline]
pub const fn size_to_large_bin(size: usize) -> usize {
    if size <= 512 {
        SMALL_BIN_COUNT
    } else if size <= 1024 {
        SMALL_BIN_COUNT + 1
    } else if size <= 2048 {
        SMALL_BIN_COUNT + 2
    } else if size <= 4096 {
        SMALL_BIN_COUNT + 3
    } else if size <= 8192 {
        SMALL_BIN_COUNT + 4
    } else if size <= 16384 {
        SMALL_BIN_COUNT + 5
    } else if size <= 32768 {
        SMALL_BIN_COUNT + 6
    } else {
        SMALL_BIN_COUNT + 7
    }
}

#[inline]
pub fn size_to_bin(size: usize) -> usize {
    size_to_small_bin(size).unwrap_or_else(|| size_to_large_bin(size))
}
