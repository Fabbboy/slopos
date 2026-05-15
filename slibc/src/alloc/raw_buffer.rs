//! Owning heap buffer with safe byte-indexed access.
//!
//! Wraps a `slibc::mem::malloc::alloc`'d region. All raw-pointer
//! arithmetic lives inside this module; consumers see only safe
//! `write_byte` / `read_byte` / `fill_with` / `verify` / `realloc`.

use core::ffi::c_void;
use core::mem;

use crate::mem::malloc;

/// Heap allocation owning a single `malloc::alloc`'d region.
///
/// `Drop` releases the region via `malloc::dealloc`. The buffer is
/// allocated from slibc's dlmalloc; size requests of 0 still allocate
/// (matching libc behaviour) so `len()` always reflects the requested
/// size.
pub struct RawBuffer {
    ptr: *mut u8,
    len: usize,
}

impl RawBuffer {
    /// Allocate `len` bytes. Returns `None` on allocation failure.
    pub fn new(len: usize) -> Option<Self> {
        let ptr = malloc::alloc(len).cast::<u8>();
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, len })
        }
    }

    /// Allocate and zero `len` bytes.
    pub fn new_zeroed(len: usize) -> Option<Self> {
        let ptr = malloc::calloc(1, len).cast::<u8>();
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, len })
        }
    }

    /// Resize to `new_len` bytes. Preserves the prefix common to both
    /// sizes. Returns `None` on allocation failure (the original buffer
    /// is freed by `realloc` per libc semantics, so `self` becomes
    /// invalid either way — `mem::forget` suppresses the double-free).
    pub fn realloc(self, new_len: usize) -> Option<Self> {
        let old_ptr = self.ptr;
        mem::forget(self);
        let new_ptr = malloc::realloc(old_ptr.cast::<c_void>(), new_len).cast::<u8>();
        if new_ptr.is_null() {
            None
        } else {
            Some(Self {
                ptr: new_ptr,
                len: new_len,
            })
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Write a single byte. Panics if `i >= self.len()`.
    pub fn write_byte(&mut self, i: usize, v: u8) {
        assert!(i < self.len, "RawBuffer::write_byte index OOB");
        // SAFETY: bounds-checked above; `ptr` is valid for `len` bytes
        // and we have exclusive access via `&mut self`.
        unsafe {
            self.ptr.add(i).write(v);
        }
    }

    /// Read a single byte. Panics if `i >= self.len()`.
    pub fn read_byte(&self, i: usize) -> u8 {
        assert!(i < self.len, "RawBuffer::read_byte index OOB");
        // SAFETY: bounds-checked above; `ptr` is valid for `len` bytes
        // and we hold a shared borrow.
        unsafe { self.ptr.add(i).read() }
    }

    /// Fill the buffer by applying `f(i)` over every index.
    pub fn fill_with<F: FnMut(usize) -> u8>(&mut self, mut f: F) {
        for i in 0..self.len {
            self.write_byte(i, f(i));
        }
    }

    /// Verify every byte matches `f(i)`. Returns false on first mismatch.
    pub fn verify<F: FnMut(usize) -> u8>(&self, mut f: F) -> bool {
        for i in 0..self.len {
            if self.read_byte(i) != f(i) {
                return false;
            }
        }
        true
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            malloc::dealloc(self.ptr.cast::<c_void>());
        }
    }
}
