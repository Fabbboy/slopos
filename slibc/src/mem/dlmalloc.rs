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
use crate::pal::syscall::sys_brk;

const INITIAL_HEAP_SIZE: usize = 64 * 1024;
const EXTEND_MIN_SIZE: usize = 64 * 1024;
const PAGE_SIZE: usize = 4096;
const SHRINK_THRESHOLD: usize = 256 * 1024;
const MIN_WILDERNESS: usize = 64 * 1024;
const HEAP_EDGE_PAD: usize = chunk::HEADER_SIZE;
const MMAP_SUFFIX_PAD: usize = chunk::HEADER_SIZE;
const UNSORTED_DRAIN_LIMIT: usize = 10;

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

pub struct DlMalloc {
    bins: BinArray,
    top: ChunkPtr,
    top_size: usize,
    heap_start: *mut u8,
    heap_end: *mut u8,
    mmap_threshold: usize,
}

impl DlMalloc {
    pub const fn new() -> Self {
        Self {
            bins: BinArray::new(),
            top: ptr::null_mut(),
            top_size: 0,
            heap_start: ptr::null_mut(),
            heap_end: ptr::null_mut(),
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
            return self.alloc_mmap(request_size);
        }

        self.init();
        if self.top.is_null() {
            return ptr::null_mut();
        }

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

        if self.ensure_top_capacity(request_size) {
            return unsafe { self.allocate_from_top(request_size) };
        }

        self.alloc_mmap(request_size)
    }

    pub fn dealloc(&mut self, ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }

        let chunk_ptr = unsafe { chunk::from_data_ptr(ptr.cast::<u8>()) };
        if unsafe { chunk::is_mmap(chunk_ptr) } {
            if unsafe { self.is_valid_mmap_chunk(chunk_ptr) } {
                unsafe {
                    self.dealloc_mmap(chunk_ptr);
                }
            }
            return;
        }

        if !unsafe { self.is_allocated_heap_chunk(chunk_ptr) } {
            return;
        }

        unsafe {
            self.release_chunk(chunk_ptr);
        }
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
        let is_mmap = unsafe { chunk::is_mmap(chunk_ptr) };
        if is_mmap {
            if !unsafe { self.is_valid_mmap_chunk(chunk_ptr) } {
                return ptr::null_mut();
            }
        } else if !unsafe { self.is_allocated_heap_chunk(chunk_ptr) } {
            return ptr::null_mut();
        }

        let Some(request_size) = Self::request_size(new_size) else {
            return ptr::null_mut();
        };

        let old_size = unsafe { chunk::size(chunk_ptr) };
        if old_size >= request_size {
            if !is_mmap && old_size.saturating_sub(request_size) >= chunk::MIN_CHUNK_SIZE {
                unsafe {
                    self.shrink_in_place(chunk_ptr, request_size);
                }
            }
            return ptr;
        }

        if !is_mmap {
            let next = unsafe { chunk::next_physical(chunk_ptr) };
            if next == self.top {
                let needed = request_size
                    .saturating_add(chunk::MIN_CHUNK_SIZE)
                    .saturating_sub(old_size.saturating_add(self.top_size));
                if self.ensure_top_capacity(request_size) || (needed == 0 && self.top_size != 0) {
                    return unsafe { self.grow_into_top(chunk_ptr, request_size) };
                }
            }

            if next != self.top && unsafe { self.is_free_chunk(next) } {
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

        self.alloc_mmap_aligned(request_size, alignment)
    }

    pub fn malloc_usable_size(&mut self, ptr: *mut u8) -> usize {
        if ptr.is_null() {
            return 0;
        }

        let chunk_ptr = unsafe { chunk::from_data_ptr(ptr) };
        if unsafe { chunk::is_mmap(chunk_ptr) } {
            if unsafe { self.is_valid_mmap_chunk(chunk_ptr) } {
                return unsafe { chunk::usable_size(chunk_ptr) };
            }
            return 0;
        }

        if unsafe { self.is_allocated_heap_chunk(chunk_ptr) } {
            unsafe { chunk::usable_size(chunk_ptr) }
        } else {
            0
        }
    }

    fn init(&mut self) {
        if !self.heap_start.is_null() {
            return;
        }

        let current_brk = sys_brk(ptr::null_mut()).cast::<u8>();
        if current_brk.is_null() || current_brk as usize == usize::MAX {
            return;
        }

        let Some(new_brk_addr) = (current_brk as usize).checked_add(INITIAL_HEAP_SIZE) else {
            return;
        };
        let new_brk = new_brk_addr as *mut c_void;
        let result = sys_brk(new_brk).cast::<u8>();
        if result != new_brk.cast::<u8>() {
            return;
        }

        let first_chunk = unsafe { current_brk.add(HEAP_EDGE_PAD) };
        let top_size = INITIAL_HEAP_SIZE.saturating_sub(HEAP_EDGE_PAD * 2);
        if top_size < chunk::MIN_CHUNK_SIZE || u32::try_from(top_size).is_err() {
            return;
        }

        self.heap_start = current_brk;
        self.heap_end = result;
        self.top = first_chunk;
        self.top_size = top_size;

        unsafe {
            chunk::set_prev_size(self.top, 0);
            chunk::set_size_flags(self.top, self.top_size, chunk::PREV_IN_USE);
            chunk::write_footer(self.top);
        }
    }

    fn ensure_top_capacity(&mut self, request_size: usize) -> bool {
        if self.top_size >= request_size.saturating_add(chunk::MIN_CHUNK_SIZE) {
            return true;
        }

        let needed = request_size
            .saturating_add(chunk::MIN_CHUNK_SIZE)
            .saturating_sub(self.top_size);
        self.extend_heap(needed)
    }

    fn extend_heap(&mut self, min_extra: usize) -> bool {
        if self.top.is_null() {
            self.init();
        }
        if self.top.is_null() {
            return false;
        }

        let extend_size = align_up_usize(min_extra.max(EXTEND_MIN_SIZE), chunk::ALIGNMENT);
        if extend_size == 0 {
            return false;
        }

        let Some(new_brk_addr) = (self.heap_end as usize).checked_add(extend_size) else {
            return false;
        };
        let new_brk = new_brk_addr as *mut c_void;
        let result = sys_brk(new_brk).cast::<u8>();
        if result != new_brk.cast::<u8>() {
            return false;
        }

        let Some(new_top_size) = self.top_size.checked_add(extend_size) else {
            return false;
        };
        if u32::try_from(new_top_size).is_err() {
            return false;
        }

        self.heap_end = result;
        self.top_size = new_top_size;
        unsafe {
            let flags = chunk::flags(self.top) & chunk::PREV_IN_USE;
            chunk::set_size_flags(self.top, self.top_size, flags);
            chunk::write_footer(self.top);
        }
        true
    }

    fn try_shrink_heap(&mut self) {
        if self.top_size <= SHRINK_THRESHOLD || self.top.is_null() {
            return;
        }
        let release = (self.top_size - MIN_WILDERNESS) & !(PAGE_SIZE - 1);
        if release < PAGE_SIZE {
            return;
        }
        let new_end = unsafe { self.top.add(self.top_size - release) };
        let result = sys_brk(new_end.cast::<c_void>()).cast::<u8>();
        if result == new_end {
            self.heap_end = new_end;
            self.top_size -= release;
            let flags = unsafe { chunk::flags(self.top) } & chunk::PREV_IN_USE;
            unsafe {
                chunk::set_size_flags(self.top, self.top_size, flags);
                chunk::write_footer(self.top);
            }
        }
    }

    fn alloc_mmap(&mut self, request_size: usize) -> *mut c_void {
        self.alloc_mmap_aligned(request_size, chunk::ALIGNMENT)
            .cast::<c_void>()
    }

    fn alloc_mmap_aligned(&mut self, request_size: usize, alignment: usize) -> *mut u8 {
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

        let ret = unsafe {
            syscall6(
                SYSCALL_MMAP,
                0,
                mapping_len as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                (-1i64) as u64,
                0,
            )
        };

        match crate::demux(ret) {
            Ok(addr) => {
                let base = addr as *mut u8;
                let aligned_data =
                    align_up_usize(unsafe { base.add(chunk::HEADER_SIZE) } as usize, alignment);
                let chunk_ptr = (aligned_data - chunk::HEADER_SIZE) as ChunkPtr;
                let offset = (chunk_ptr as usize).saturating_sub(base as usize);
                let chunk_size = mapping_len
                    .saturating_sub(offset)
                    .saturating_sub(MMAP_SUFFIX_PAD);
                if offset < HEAP_EDGE_PAD
                    || chunk_size < request_size
                    || chunk_size < chunk::MIN_CHUNK_SIZE
                    || chunk_size & (chunk::ALIGNMENT - 1) != 0
                    || u32::try_from(offset).is_err()
                    || u32::try_from(chunk_size).is_err()
                {
                    let _ = unsafe { syscall2(SYSCALL_MUNMAP, base as u64, mapping_len as u64) };
                    return ptr::null_mut();
                }

                unsafe {
                    chunk::set_prev_size(chunk_ptr, offset);
                    chunk::set_size_flags(
                        chunk_ptr,
                        chunk_size,
                        chunk::PREV_IN_USE | chunk::MMAP_CHUNK,
                    );
                }
                aligned_data as *mut u8
            }
            Err(_) => ptr::null_mut(),
        }
    }

    unsafe fn dealloc_mmap(&mut self, chunk_ptr: ChunkPtr) {
        let offset = unsafe { chunk::prev_size(chunk_ptr) };
        let total = unsafe { chunk::size(chunk_ptr) }
            .saturating_add(offset)
            .saturating_add(MMAP_SUFFIX_PAD);
        let base = unsafe { chunk_ptr.sub(offset) };
        let _ = unsafe { syscall2(SYSCALL_MUNMAP, base as u64, total as u64) };
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

    unsafe fn allocate_from_top(&mut self, request_size: usize) -> *mut c_void {
        let old_top = self.top;
        let old_top_size = self.top_size;
        let prev_flags = unsafe { chunk::flags(old_top) } & chunk::PREV_IN_USE;

        let new_top = unsafe { old_top.add(request_size) };
        let new_top_size = old_top_size.saturating_sub(request_size);
        debug_assert!(new_top_size >= chunk::MIN_CHUNK_SIZE);

        self.top = new_top;
        self.top_size = new_top_size;

        unsafe {
            chunk::set_size_flags(old_top, request_size, prev_flags);
            chunk::set_prev_size(new_top, request_size);
            chunk::set_size_flags(new_top, new_top_size, chunk::PREV_IN_USE);
            chunk::write_footer(new_top);
            chunk::data_ptr(old_top).cast::<c_void>()
        }
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
        if next != self.top && unsafe { self.is_free_chunk(next) } {
            unsafe {
                self.bins.remove(next);
            }
            merged_size = merged_size.saturating_add(unsafe { chunk::size(next) });
        }

        if !unsafe { chunk::is_prev_in_use(merged) } {
            let prev = unsafe { chunk::prev_physical(merged) };
            unsafe {
                self.bins.remove(prev);
            }
            merged_size = merged_size.saturating_add(unsafe { chunk::size(prev) });
            merged = prev;
        }

        let merged_flags = unsafe { chunk::flags(merged) } & chunk::PREV_IN_USE;
        unsafe {
            chunk::set_size_flags(merged, merged_size, merged_flags);
        }

        if unsafe { chunk::next_physical(merged) } == self.top {
            self.top = merged;
            self.top_size = self.top_size.saturating_add(merged_size);
            unsafe {
                chunk::set_size_flags(self.top, self.top_size, merged_flags);
                chunk::write_footer(self.top);
            }
            self.try_shrink_heap();
            return;
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

    unsafe fn grow_into_top(&mut self, chunk_ptr: ChunkPtr, request_size: usize) -> *mut c_void {
        let old_size = unsafe { chunk::size(chunk_ptr) };
        let total = old_size.saturating_add(self.top_size);
        if total < request_size.saturating_add(chunk::MIN_CHUNK_SIZE) {
            return ptr::null_mut();
        }

        let new_top = unsafe { chunk_ptr.add(request_size) };
        let new_top_size = total.saturating_sub(request_size);
        let prev_flags = unsafe { chunk::flags(chunk_ptr) } & chunk::PREV_IN_USE;

        self.top = new_top;
        self.top_size = new_top_size;

        unsafe {
            chunk::set_size_flags(chunk_ptr, request_size, prev_flags);
            chunk::set_prev_size(new_top, request_size);
            chunk::set_size_flags(new_top, new_top_size, chunk::PREV_IN_USE);
            chunk::write_footer(new_top);
            chunk::data_ptr(chunk_ptr).cast::<c_void>()
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

    unsafe fn set_successor_state(&self, chunk_ptr: ChunkPtr, prev_in_use: bool) {
        let next = unsafe { chunk::next_physical(chunk_ptr) };
        if (next as usize) > (self.top as usize) {
            return;
        }

        let size = unsafe { chunk::size(chunk_ptr) };
        unsafe {
            chunk::set_prev_size(next, size);
            chunk::set_prev_in_use(next, prev_in_use);
        }
    }

    unsafe fn is_free_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        if chunk_ptr.is_null() {
            return false;
        }
        if chunk_ptr == self.top {
            return true;
        }

        let next = unsafe { chunk::next_physical(chunk_ptr) };
        if (next as usize) > (self.top as usize) {
            return false;
        }
        !unsafe { chunk::is_prev_in_use(next) }
    }

    unsafe fn is_allocated_heap_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        if !unsafe { self.chunk_in_heap(chunk_ptr) } {
            return false;
        }

        let size = unsafe { chunk::size(chunk_ptr) };
        if !Self::is_valid_chunk_size(size) {
            return false;
        }

        let next = unsafe { chunk::next_physical(chunk_ptr) };
        if (next as usize) > (self.top as usize) {
            return false;
        }

        unsafe { chunk::is_prev_in_use(next) }
    }

    unsafe fn is_valid_mmap_chunk(&self, chunk_ptr: ChunkPtr) -> bool {
        if chunk_ptr.is_null() || !unsafe { chunk::is_mmap(chunk_ptr) } {
            return false;
        }

        let size = unsafe { chunk::size(chunk_ptr) };
        let offset = unsafe { chunk::prev_size(chunk_ptr) };
        Self::is_valid_chunk_size(size)
            && offset >= HEAP_EDGE_PAD
            && (size.saturating_add(offset).saturating_add(MMAP_SUFFIX_PAD) & (PAGE_SIZE - 1) == 0)
    }

    unsafe fn chunk_in_heap(&self, chunk_ptr: ChunkPtr) -> bool {
        if self.top.is_null() {
            return false;
        }
        let heap_base = unsafe { self.heap_start.add(HEAP_EDGE_PAD) } as usize;
        let chunk_addr = chunk_ptr as usize;
        chunk_addr >= heap_base && chunk_addr < self.top as usize
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

    pub fn heap_size(&self) -> usize {
        if self.heap_start.is_null() {
            0
        } else {
            (self.heap_end as usize).saturating_sub(self.heap_start as usize)
        }
    }

    pub fn wilderness_size(&self) -> usize {
        self.top_size
    }

    pub fn count_mmap_chunks(&self) -> usize {
        unsafe { self.bins.count_mmap_chunks() }
    }
}

impl Default for DlMalloc {
    fn default() -> Self {
        Self::new()
    }
}
