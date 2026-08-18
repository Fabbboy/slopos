//! Segment-based dlmalloc over anonymous mmap.
//!
//! The arena is a set of independently mmap'd segments; there is no
//! program break and no contiguous-heap assumption. Each segment is
//! laid out as `[lead pad | chunks ... | fence]`: the lead pad places
//! the first chunk so its data is 16-byte aligned, and the fence is a
//! size-0 pseudo chunk header whose PREV_IN_USE bit tracks the last
//! real chunk. A fence fails every chunk validity check, so merging
//! and frees can never walk across a segment boundary — even when the
//! kernel's first-fit gap finder places two segments back to back.
//!
//! Allocations at or above `mmap_threshold` (and all over-aligned
//! `memalign` requests) get a dedicated mapping, tracked in the
//! `DirectRegion` registry.

use core::cell::SyncUnsafeCell;
use core::cmp;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::alignment::align_up_usize;
use slopos_abi::syscall::{
    MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, SYSCALL_MMAP, SYSCALL_MUNMAP,
};

use super::bins::{self, BIN_COUNT, BinArray, LARGE_BIN_COUNT, SMALL_BIN_COUNT};
use super::chunk::{self, ChunkPtr};
use crate::pal::raw::{syscall2, syscall6};

const PAGE_SIZE: usize = 4096;
const UNSORTED_DRAIN_LIMIT: usize = 10;

/// Address-space floor for a fresh arena segment. The kernel maps
/// anonymous pages lazily, so an untouched tail costs no RAM.
const SEGMENT_MIN_LEN: usize = 1024 * 1024;
/// Ceiling keeping every chunk size comfortably inside the u32 size
/// field of the chunk header.
const SEGMENT_MAX_LEN: usize = 256 * 1024 * 1024;
const MAX_SEGMENTS: usize = 32;

/// Alignment spacer at a segment base: the first chunk header sits at
/// `base + 8` so its data (`chunk + 8`) lands 16-byte aligned.
const SEGMENT_LEAD_PAD: usize = chunk::HEADER_SIZE;
const SEGMENT_FENCE_LEN: usize = chunk::HEADER_SIZE;
const SEGMENT_OVERHEAD: usize = SEGMENT_LEAD_PAD + SEGMENT_FENCE_LEN;

const MMAP_SUFFIX_PAD: usize = chunk::HEADER_SIZE;

#[repr(transparent)]
struct SyncDlMalloc(DlMalloc);

unsafe impl Sync for SyncDlMalloc {}

pub struct AllocatorHandle {
    locked: AtomicBool,
    inner: SyncUnsafeCell<SyncDlMalloc>,
}

unsafe impl Sync for AllocatorHandle {}

pub struct DlMallocGuard<'a> {
    lock: &'a AtomicBool,
    allocator: &'a mut DlMalloc,
}

impl AllocatorHandle {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: SyncUnsafeCell::new(SyncDlMalloc(DlMalloc::new())),
        }
    }

    pub fn lock(&self) -> DlMallocGuard<'_> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        DlMallocGuard {
            lock: &self.locked,
            allocator: unsafe { &mut (*self.inner.get()).0 },
        }
    }
}

impl Deref for DlMallocGuard<'_> {
    type Target = DlMalloc;

    fn deref(&self) -> &Self::Target {
        self.allocator
    }
}

impl DerefMut for DlMallocGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.allocator
    }
}

impl Drop for DlMallocGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}

pub static ALLOCATOR: AllocatorHandle = AllocatorHandle::new();

#[derive(Clone, Copy)]
struct Segment {
    base: *mut u8,
    len: usize,
}

impl Segment {
    const EMPTY: Self = Self {
        base: ptr::null_mut(),
        len: 0,
    };

    #[inline]
    fn first_chunk(&self) -> ChunkPtr {
        unsafe { self.base.add(SEGMENT_LEAD_PAD) }
    }

    #[inline]
    fn fence(&self) -> ChunkPtr {
        unsafe { self.base.add(self.len - SEGMENT_FENCE_LEN) }
    }

    #[inline]
    fn spanning_chunk_size(&self) -> usize {
        self.len - SEGMENT_OVERHEAD
    }

    /// A plausible chunk position: inside `[first_chunk, fence)` and on the
    /// 16-byte chunk grid.
    #[inline]
    fn contains_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        let addr = chunk_ptr as usize;
        let first = self.first_chunk() as usize;
        let fence = self.fence() as usize;
        addr >= first && addr < fence && (addr - first) & (chunk::ALIGNMENT - 1) == 0
    }
}

/// Registry node for one direct (whole-mapping) allocation. Nodes live
/// in arena chunks, never inside the registered mapping, so user
/// buffer overruns cannot reach registry metadata.
struct DirectRegion {
    base: *mut u8,
    len: usize,
    chunk: ChunkPtr,
    next: *mut DirectRegion,
}

pub struct DlMalloc {
    bins: BinArray,
    segments: [Segment; MAX_SEGMENTS],
    segment_count: usize,
    direct_head: *mut DirectRegion,
    mmap_threshold: usize,
}

impl DlMalloc {
    pub const fn new() -> Self {
        Self {
            bins: BinArray::new(),
            segments: [Segment::EMPTY; MAX_SEGMENTS],
            segment_count: 0,
            direct_head: ptr::null_mut(),
            mmap_threshold: 128 * 1024,
        }
    }

    pub fn alloc(&mut self, size: usize) -> *mut c_void {
        if size == 0 {
            return ptr::null_mut();
        }

        let Some(request_size) = Self::request_size(size) else {
            return ptr::null_mut();
        };

        if request_size >= self.mmap_threshold {
            return self
                .alloc_direct(request_size, chunk::ALIGNMENT)
                .cast::<c_void>();
        }

        let from_arena = self.arena_alloc(request_size);
        if !from_arena.is_null() {
            return from_arena;
        }

        self.alloc_direct(request_size, chunk::ALIGNMENT)
            .cast::<c_void>()
    }

    pub fn dealloc(&mut self, ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }

        let chunk_ptr = unsafe { chunk::from_data_ptr(ptr.cast::<u8>()) };
        // Validate before any header read: segment membership bounds the arena
        // reads, and the direct registry compares pointer values only.
        if unsafe { self.is_allocated_heap_chunk(chunk_ptr) } {
            unsafe {
                self.release_chunk(chunk_ptr);
            }
            return;
        }

        let _ = unsafe { self.release_direct(chunk_ptr) };
    }

    pub fn realloc(&mut self, ptr: *mut c_void, new_size: usize) -> *mut c_void {
        if ptr.is_null() {
            return self.alloc(new_size);
        }

        if new_size == 0 {
            self.dealloc(ptr);
            return ptr::null_mut();
        }

        let chunk_ptr = unsafe { chunk::from_data_ptr(ptr.cast::<u8>()) };
        let in_arena = unsafe { self.is_allocated_heap_chunk(chunk_ptr) };
        if !in_arena && self.find_direct(chunk_ptr).is_null() {
            return ptr::null_mut();
        }

        let Some(request_size) = Self::request_size(new_size) else {
            return ptr::null_mut();
        };

        let old_size = unsafe { chunk::size(chunk_ptr) };
        if old_size >= request_size {
            if in_arena && old_size.saturating_sub(request_size) >= chunk::MIN_CHUNK_SIZE {
                unsafe {
                    self.shrink_in_place(chunk_ptr, request_size);
                }
            }
            return ptr;
        }

        if in_arena {
            let next = unsafe { chunk::next_physical(chunk_ptr) };
            if unsafe { self.is_free_chunk(next) } {
                let combined = old_size.saturating_add(unsafe { chunk::size(next) });
                if combined >= request_size {
                    unsafe {
                        self.bins.remove(next);
                    }
                    return unsafe { self.grow_into_next_free(chunk_ptr, request_size, combined) };
                }
            }
        }

        let new_ptr = self.alloc(new_size);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        let copy_size = cmp::min(unsafe { chunk::usable_size(chunk_ptr) }, new_size);
        unsafe {
            ptr::copy_nonoverlapping(ptr.cast::<u8>(), new_ptr.cast::<u8>(), copy_size);
        }
        self.dealloc(ptr);

        new_ptr
    }

    pub fn memalign(&mut self, alignment: usize, size: usize) -> *mut u8 {
        if size == 0 {
            return ptr::null_mut();
        }

        if alignment <= chunk::ALIGNMENT {
            return self.alloc(size).cast::<u8>();
        }

        if !alignment.is_power_of_two() {
            return ptr::null_mut();
        }

        let Some(request_size) = Self::request_size(size) else {
            return ptr::null_mut();
        };

        self.alloc_direct(request_size, alignment)
    }

    pub fn malloc_usable_size(&mut self, ptr: *mut u8) -> usize {
        if ptr.is_null() {
            return 0;
        }

        let chunk_ptr = unsafe { chunk::from_data_ptr(ptr) };
        if unsafe { self.is_allocated_heap_chunk(chunk_ptr) }
            || !self.find_direct(chunk_ptr).is_null()
        {
            unsafe { chunk::usable_size(chunk_ptr) }
        } else {
            0
        }
    }

    fn arena_alloc(&mut self, request_size: usize) -> *mut c_void {
        if let Some(bin_idx) = bins::size_to_small_bin(request_size)
            && !self.bins.is_empty(bin_idx)
        {
            let chunk_ptr = unsafe { self.bins.pop_front(bin_idx) };
            if !chunk_ptr.is_null() {
                return unsafe { self.allocate_from_chunk(chunk_ptr, request_size) };
            }
        }

        let unsorted_match = unsafe { self.drain_unsorted(request_size) };
        if !unsorted_match.is_null() {
            return unsorted_match;
        }

        if let Some(bin_idx) = bins::size_to_small_bin(request_size)
            && let Some(found_idx) = self.bins.first_nonempty_from(bin_idx)
            && found_idx < SMALL_BIN_COUNT
        {
            let chunk_ptr = unsafe { self.bins.pop_front(found_idx) };
            if !chunk_ptr.is_null() {
                return unsafe { self.allocate_from_chunk(chunk_ptr, request_size) };
            }
        }

        let large_fit = unsafe { self.find_large_fit(request_size) };
        if !large_fit.is_null() {
            return unsafe { self.allocate_from_chunk(large_fit, request_size) };
        }

        let fresh = self.allocate_segment(request_size);
        if fresh.is_null() {
            return ptr::null_mut();
        }
        unsafe { self.allocate_from_chunk(fresh, request_size) }
    }

    /// Map a fresh arena segment sized to serve `request_size`, format it as
    /// one spanning free chunk guarded by the end fence, and record it.
    /// Returns that chunk (not yet binned) or null. Segments grow
    /// geometrically, so the fixed table cannot fill before the address space.
    fn allocate_segment(&mut self, request_size: usize) -> ChunkPtr {
        if self.segment_count >= MAX_SEGMENTS {
            return ptr::null_mut();
        }

        let Some(needed) = request_size.checked_add(SEGMENT_OVERHEAD) else {
            return ptr::null_mut();
        };
        let arena_total: usize = self.segments[..self.segment_count]
            .iter()
            .map(|s| s.len)
            .sum();
        let target = needed
            .max(SEGMENT_MIN_LEN)
            .max(arena_total / 2)
            .min(SEGMENT_MAX_LEN)
            .max(needed);
        let len = align_up_usize(target, PAGE_SIZE);
        if len < needed {
            return ptr::null_mut();
        }

        let seg = Segment {
            base: Self::mmap_anon(len),
            len,
        };
        if seg.base.is_null() {
            return ptr::null_mut();
        }

        let first = seg.first_chunk();
        let chunk_size = seg.spanning_chunk_size();
        if !Self::is_valid_chunk_size(chunk_size) || u32::try_from(chunk_size).is_err() {
            Self::munmap(seg.base, seg.len);
            return ptr::null_mut();
        }

        unsafe {
            chunk::set_prev_size(first, 0);
            chunk::set_size_flags(first, chunk_size, chunk::PREV_IN_USE);
            chunk::write_footer(first);
            let fence = seg.fence();
            chunk::set_prev_size(fence, chunk_size);
            chunk::set_size_flags(fence, 0, 0);
        }

        self.segments[self.segment_count] = seg;
        self.segment_count += 1;
        first
    }

    #[inline]
    fn containing_segment(&self, chunk_ptr: ChunkPtr) -> Option<usize> {
        (0..self.segment_count).find(|&i| self.segments[i].contains_chunk(chunk_ptr))
    }

    /// A spanning free chunk releases its whole segment back to the kernel,
    /// except the last default-sized one: that stays resident as the warm
    /// arena so a free/alloc cycle does not thrash mmap/munmap.
    fn should_release_segment(&self, idx: usize) -> bool {
        self.segment_count > 1 || self.segments[idx].len > SEGMENT_MIN_LEN
    }

    fn remove_segment(&mut self, idx: usize) {
        self.segment_count -= 1;
        self.segments[idx] = self.segments[self.segment_count];
        self.segments[self.segment_count] = Segment::EMPTY;
    }

    fn alloc_direct(&mut self, request_size: usize, alignment: usize) -> *mut u8 {
        let Some(len_request) = request_size
            .checked_add(alignment)
            .and_then(|value| value.checked_add(MMAP_SUFFIX_PAD))
        else {
            return ptr::null_mut();
        };
        let mapping_len = align_up_usize(len_request, PAGE_SIZE);
        if mapping_len == 0 {
            return ptr::null_mut();
        }

        let base = Self::mmap_anon(mapping_len);
        if base.is_null() {
            return ptr::null_mut();
        }

        let aligned_data =
            align_up_usize(unsafe { base.add(chunk::HEADER_SIZE) } as usize, alignment);
        let chunk_ptr = (aligned_data - chunk::HEADER_SIZE) as ChunkPtr;
        let offset = (chunk_ptr as usize).saturating_sub(base as usize);
        let chunk_size = mapping_len
            .saturating_sub(offset)
            .saturating_sub(MMAP_SUFFIX_PAD);
        if offset < chunk::HEADER_SIZE
            || chunk_size < request_size
            || !Self::is_valid_chunk_size(chunk_size)
            || u32::try_from(offset).is_err()
            || u32::try_from(chunk_size).is_err()
        {
            Self::munmap(base, mapping_len);
            return ptr::null_mut();
        }

        let node = self.alloc_registry_node();
        if node.is_null() {
            Self::munmap(base, mapping_len);
            return ptr::null_mut();
        }

        unsafe {
            chunk::set_prev_size(chunk_ptr, offset);
            chunk::set_size_flags(
                chunk_ptr,
                chunk_size,
                chunk::PREV_IN_USE | chunk::MMAP_CHUNK,
            );
            node.write(DirectRegion {
                base,
                len: mapping_len,
                chunk: chunk_ptr,
                next: self.direct_head,
            });
        }
        self.direct_head = node;

        aligned_data as *mut u8
    }

    fn alloc_registry_node(&mut self) -> *mut DirectRegion {
        let Some(request_size) = Self::request_size(size_of::<DirectRegion>()) else {
            return ptr::null_mut();
        };
        self.arena_alloc(request_size).cast::<DirectRegion>()
    }

    fn find_direct(&self, chunk_ptr: ChunkPtr) -> *mut DirectRegion {
        let mut cur = self.direct_head;
        while !cur.is_null() {
            if unsafe { (*cur).chunk } == chunk_ptr {
                return cur;
            }
            cur = unsafe { (*cur).next };
        }
        ptr::null_mut()
    }

    /// Unmap the direct mapping registered for `chunk_ptr` and free its
    /// registry node. The registry is the source of truth: a pointer with no
    /// node is ignored.
    unsafe fn release_direct(&mut self, chunk_ptr: ChunkPtr) -> bool {
        let mut link: *mut *mut DirectRegion = &mut self.direct_head;
        loop {
            let cur = unsafe { *link };
            if cur.is_null() {
                return false;
            }
            if unsafe { (*cur).chunk } == chunk_ptr {
                let DirectRegion {
                    base, len, next, ..
                } = unsafe { cur.read() };
                unsafe {
                    *link = next;
                    self.release_chunk(chunk::from_data_ptr(cur.cast::<u8>()));
                }
                Self::munmap(base, len);
                return true;
            }
            link = unsafe { &mut (*cur).next };
        }
    }

    unsafe fn allocate_from_chunk(
        &mut self,
        chunk_ptr: ChunkPtr,
        request_size: usize,
    ) -> *mut c_void {
        let chunk_size = unsafe { chunk::size(chunk_ptr) };
        let prev_flags = unsafe { chunk::flags(chunk_ptr) } & chunk::PREV_IN_USE;
        let remainder_size = chunk_size.saturating_sub(request_size);

        if remainder_size >= chunk::MIN_CHUNK_SIZE {
            let remainder = unsafe { chunk_ptr.add(request_size) };
            unsafe {
                chunk::set_size_flags(chunk_ptr, request_size, prev_flags);
                chunk::set_prev_size(remainder, request_size);
                chunk::set_size_flags(remainder, remainder_size, chunk::PREV_IN_USE);
                chunk::write_footer(remainder);
                self.set_successor_state(remainder, false);
                self.bins.insert_unsorted(remainder);
            }
        } else {
            unsafe {
                chunk::set_size_flags(chunk_ptr, chunk_size, prev_flags);
                self.set_successor_state(chunk_ptr, true);
            }
        }

        unsafe { chunk::data_ptr(chunk_ptr).cast::<c_void>() }
    }

    unsafe fn drain_unsorted(&mut self, request_size: usize) -> *mut c_void {
        let mut drained = 0usize;
        while drained < UNSORTED_DRAIN_LIMIT && !self.bins.unsorted_is_empty() {
            let chunk_ptr = unsafe { self.bins.pop_unsorted_front() };
            if chunk_ptr.is_null() {
                break;
            }

            if unsafe { chunk::size(chunk_ptr) } == request_size {
                return unsafe { self.allocate_from_chunk(chunk_ptr, request_size) };
            }

            unsafe {
                self.classify_free_chunk(chunk_ptr);
            }
            drained += 1;
        }

        ptr::null_mut()
    }

    unsafe fn classify_free_chunk(&mut self, chunk_ptr: ChunkPtr) {
        let bin_idx = bins::size_to_bin(unsafe { chunk::size(chunk_ptr) });
        unsafe {
            self.bins.insert(bin_idx, chunk_ptr);
        }
    }

    unsafe fn find_large_fit(&mut self, request_size: usize) -> ChunkPtr {
        let start_idx = bins::size_to_large_bin(request_size);
        let best_fit = unsafe { self.bins.find_best_fit(start_idx, request_size) };
        if !best_fit.is_null() {
            unsafe {
                self.bins.remove(best_fit);
            }
            return best_fit;
        }

        let mut current = start_idx.saturating_add(1);
        while current < BIN_COUNT {
            if current < SMALL_BIN_COUNT || current >= SMALL_BIN_COUNT + LARGE_BIN_COUNT {
                current += 1;
                continue;
            }

            let next_idx = self.bins.first_nonempty_from(current);
            let Some(found_idx) = next_idx else {
                break;
            };
            if found_idx < SMALL_BIN_COUNT {
                current = SMALL_BIN_COUNT;
                continue;
            }

            let chunk_ptr = unsafe { self.bins.pop_front(found_idx) };
            if !chunk_ptr.is_null() {
                return chunk_ptr;
            }
            current = found_idx.saturating_add(1);
        }

        ptr::null_mut()
    }

    unsafe fn release_chunk(&mut self, chunk_ptr: ChunkPtr) {
        let mut merged = chunk_ptr;
        let mut merged_size = unsafe { chunk::size(chunk_ptr) };

        let next = unsafe { chunk::next_physical(merged) };
        if unsafe { self.is_free_chunk(next) } {
            unsafe {
                self.bins.remove(next);
            }
            merged_size = merged_size.saturating_add(unsafe { chunk::size(next) });
        }

        if !unsafe { chunk::is_prev_in_use(merged) } {
            let prev = unsafe { chunk::prev_physical(merged) };
            // Merge backward only if prev's own size agrees with the
            // prev_size that led here; a corrupted neighbour is leaked.
            if self.containing_segment(prev).is_some()
                && unsafe { chunk::next_physical(prev) } == merged
            {
                unsafe {
                    self.bins.remove(prev);
                }
                merged_size = merged_size.saturating_add(unsafe { chunk::size(prev) });
                merged = prev;
            }
        }

        let merged_flags = unsafe { chunk::flags(merged) } & chunk::PREV_IN_USE;
        unsafe {
            chunk::set_size_flags(merged, merged_size, merged_flags);
        }

        if let Some(idx) = self.containing_segment(merged) {
            let seg = self.segments[idx];
            if merged == seg.first_chunk()
                && merged_size == seg.spanning_chunk_size()
                && self.should_release_segment(idx)
            {
                self.remove_segment(idx);
                Self::munmap(seg.base, seg.len);
                return;
            }
        }

        unsafe {
            chunk::write_footer(merged);
            self.set_successor_state(merged, false);
            self.bins.insert_unsorted(merged);
        }
    }

    unsafe fn shrink_in_place(&mut self, chunk_ptr: ChunkPtr, request_size: usize) {
        let old_size = unsafe { chunk::size(chunk_ptr) };
        let tail = unsafe { chunk_ptr.add(request_size) };
        let tail_size = old_size.saturating_sub(request_size);
        let prev_flags = unsafe { chunk::flags(chunk_ptr) } & chunk::PREV_IN_USE;

        unsafe {
            chunk::set_size_flags(chunk_ptr, request_size, prev_flags);
            chunk::set_prev_size(tail, request_size);
            chunk::set_size_flags(tail, tail_size, chunk::PREV_IN_USE);
            self.release_chunk(tail);
        }
    }

    unsafe fn grow_into_next_free(
        &mut self,
        chunk_ptr: ChunkPtr,
        request_size: usize,
        total_size: usize,
    ) -> *mut c_void {
        let prev_flags = unsafe { chunk::flags(chunk_ptr) } & chunk::PREV_IN_USE;
        let remainder_size = total_size.saturating_sub(request_size);

        if remainder_size >= chunk::MIN_CHUNK_SIZE {
            let remainder = unsafe { chunk_ptr.add(request_size) };
            unsafe {
                chunk::set_size_flags(chunk_ptr, request_size, prev_flags);
                chunk::set_prev_size(remainder, request_size);
                chunk::set_size_flags(remainder, remainder_size, chunk::PREV_IN_USE);
                self.release_chunk(remainder);
            }
        } else {
            unsafe {
                chunk::set_size_flags(chunk_ptr, total_size, prev_flags);
                self.set_successor_state(chunk_ptr, true);
            }
        }

        unsafe { chunk::data_ptr(chunk_ptr).cast::<c_void>() }
    }

    /// Record `chunk_ptr`'s size and in-use state in its physical successor's
    /// header. That successor is always writable: a real chunk, or the fence.
    unsafe fn set_successor_state(&self, chunk_ptr: ChunkPtr, prev_in_use: bool) {
        let next = unsafe { chunk::next_physical(chunk_ptr) };
        let size = unsafe { chunk::size(chunk_ptr) };
        unsafe {
            chunk::set_prev_size(next, size);
            chunk::set_prev_in_use(next, prev_in_use);
        }
    }

    /// A chunk is free iff its successor's PREV_IN_USE bit is clear. Fences
    /// (size 0) and out-of-grid pointers fail the segment and size checks, so
    /// merging never crosses a segment boundary.
    unsafe fn is_free_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        let Some(idx) = self.containing_segment(chunk_ptr) else {
            return false;
        };

        let size = unsafe { chunk::size(chunk_ptr) };
        if !Self::is_valid_chunk_size(size) {
            return false;
        }

        let next = unsafe { chunk::next_physical(chunk_ptr) };
        if (next as usize) > (self.segments[idx].fence() as usize) {
            return false;
        }
        !unsafe { chunk::is_prev_in_use(next) }
    }

    unsafe fn is_allocated_heap_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        let Some(idx) = self.containing_segment(chunk_ptr) else {
            return false;
        };

        let size = unsafe { chunk::size(chunk_ptr) };
        if !Self::is_valid_chunk_size(size) {
            return false;
        }

        let next = unsafe { chunk::next_physical(chunk_ptr) };
        if (next as usize) > (self.segments[idx].fence() as usize) {
            return false;
        }
        unsafe { chunk::is_prev_in_use(next) }
    }

    fn mmap_anon(len: usize) -> *mut u8 {
        let ret = unsafe {
            syscall6(
                SYSCALL_MMAP,
                0,
                len as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                (-1i64) as u64,
                0,
            )
        };
        match crate::demux(ret) {
            Ok(addr) => addr as *mut u8,
            Err(_) => ptr::null_mut(),
        }
    }

    fn munmap(base: *mut u8, len: usize) {
        let _ = unsafe { syscall2(SYSCALL_MUNMAP, base as u64, len as u64) };
    }

    fn request_size(size: usize) -> Option<usize> {
        let with_header = size.checked_add(chunk::HEADER_SIZE)?;
        let aligned = align_up_usize(with_header, chunk::ALIGNMENT);
        if aligned < with_header {
            return None;
        }

        let request = aligned.max(chunk::MIN_CHUNK_SIZE);
        if u32::try_from(request).is_err() {
            None
        } else {
            Some(request)
        }
    }

    #[inline]
    fn is_valid_chunk_size(size: usize) -> bool {
        size >= chunk::MIN_CHUNK_SIZE && size & (chunk::ALIGNMENT - 1) == 0
    }

    /// Total address space held by arena segments.
    pub fn arena_size(&self) -> usize {
        self.segments[..self.segment_count]
            .iter()
            .map(|s| s.len)
            .sum()
    }

    /// Size of the largest binned free chunk.
    pub fn largest_free_chunk(&self) -> usize {
        unsafe { self.bins.largest_chunk_size() }
    }

    /// Number of live direct (whole-mapping) allocations.
    pub fn direct_region_count(&self) -> usize {
        let mut count = 0;
        let mut cur = self.direct_head;
        while !cur.is_null() {
            count += 1;
            cur = unsafe { (*cur).next };
        }
        count
    }
}

impl Default for DlMalloc {
    fn default() -> Self {
        Self::new()
    }
}
