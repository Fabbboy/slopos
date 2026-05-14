//! HHDM page-IO byte fill / verify helpers for kernel mm tests.
//!
//! The mm/src/tests/* suites paint a known byte pattern across a freshly
//! mapped page (via the HHDM virtual address returned by the page allocator
//! or COW path), then read it back to verify the underlying physical page
//! is the one they expected. Today every site does the painting via
//! `unsafe { *ptr.add(i) = ... }` or `unsafe { core::ptr::write_bytes(...) }`;
//! these helpers fold the unsafe into one place per primitive.
//!
//! All helpers take a raw `*mut u8` / `*const u8`. Callers obtain the
//! pointer from `VirtAddr::as_mut_ptr::<u8>()` or equivalent. The
//! contract on every helper:
//!
//! - `ptr` is valid for reads (and writes, for the write helpers) of
//!   `len` consecutive bytes,
//! - the caller has exclusive access for the duration of the call
//!   (single-threaded mm-test idiom on BSP).
//!
//! Volatile variants exist for the cases where the test deliberately
//! exercises non-cacheable / dual-mapped views.

use crate::util::ptr_buf;

/// Fill `len` bytes starting at `ptr` with `byte`.
#[inline]
pub fn fill_pattern(ptr: *mut u8, byte: u8, len: usize) {
    ptr_buf::borrow_buf_mut(ptr, len).fill(byte);
}

/// Verify every byte in `[ptr, ptr+len)` equals `byte`. Returns the
/// offset of the first mismatching byte, or `None` if the whole range
/// matches.
#[inline]
pub fn verify_pattern(ptr: *const u8, byte: u8, len: usize) -> Option<usize> {
    let slice = ptr_buf::borrow_buf(ptr, len);
    slice.iter().position(|&b| b != byte)
}

/// Fill `len` bytes starting at `ptr` with `f(i)` for `i` in `0..len`.
#[inline]
pub fn fill_indexed(ptr: *mut u8, len: usize, f: impl Fn(usize) -> u8) {
    let slice = ptr_buf::borrow_buf_mut(ptr, len);
    for (i, b) in slice.iter_mut().enumerate() {
        *b = f(i);
    }
}

/// Verify every byte at offset `i` in `[ptr, ptr+len)` equals `f(i)`.
/// Returns the offset of the first mismatching byte, or `None`.
#[inline]
pub fn verify_indexed(ptr: *const u8, len: usize, f: impl Fn(usize) -> u8) -> Option<usize> {
    let slice = ptr_buf::borrow_buf(ptr, len);
    slice
        .iter()
        .enumerate()
        .find_map(|(i, &b)| if b == f(i) { None } else { Some(i) })
}

/// Write a single byte at `ptr + offset`. Non-volatile.
#[inline]
pub fn write_byte(ptr: *mut u8, offset: usize, byte: u8) {
    // SAFETY: caller upholds module-level contract; one-byte write.
    unsafe {
        ptr.add(offset).write(byte);
    }
}

/// Read a single byte at `ptr + offset`. Non-volatile.
#[inline]
pub fn read_byte(ptr: *const u8, offset: usize) -> u8 {
    // SAFETY: caller upholds module-level contract; one-byte read.
    unsafe { ptr.add(offset).read() }
}

/// Volatile fill — used by the physical-memory-pattern mm tests that
/// must defeat compiler load elision when reading back via the HHDM
/// alias.
#[inline]
pub fn fill_volatile(ptr: *mut u8, byte: u8, len: usize) {
    for i in 0..len {
        // SAFETY: caller upholds module-level contract; volatile write
        // of one byte per iteration.
        unsafe {
            ptr.add(i).write_volatile(byte);
        }
    }
}

/// Volatile single-byte write at `ptr + offset`.
#[inline]
pub fn write_volatile_byte(ptr: *mut u8, offset: usize, byte: u8) {
    // SAFETY: caller upholds module-level contract.
    unsafe {
        ptr.add(offset).write_volatile(byte);
    }
}

/// Volatile single-byte read at `ptr + offset`.
#[inline]
pub fn read_volatile_byte(ptr: *const u8, offset: usize) -> u8 {
    // SAFETY: caller upholds module-level contract.
    unsafe { ptr.add(offset).read_volatile() }
}

/// Bulk write of `byte` across `len` bytes via `core::ptr::write_bytes`.
/// Equivalent in effect to [`fill_pattern`] but emits the compiler's
/// builtin memset path (single `rep stosb` on x86_64).
#[inline]
pub fn write_bytes(ptr: *mut u8, byte: u8, len: usize) {
    // SAFETY: caller upholds module-level contract.
    unsafe {
        core::ptr::write_bytes(ptr, byte, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_then_verify_round_trips() {
        let mut buf = [0u8; 64];
        fill_pattern(buf.as_mut_ptr(), 0xA5, buf.len());
        assert!(verify_pattern(buf.as_ptr(), 0xA5, buf.len()).is_none());
    }

    #[test]
    fn verify_returns_first_mismatch_offset() {
        let mut buf = [0xA5u8; 32];
        buf[10] = 0;
        assert_eq!(verify_pattern(buf.as_ptr(), 0xA5, buf.len()), Some(10));
    }

    #[test]
    fn fill_indexed_writes_function_output() {
        let mut buf = [0u8; 16];
        fill_indexed(buf.as_mut_ptr(), buf.len(), |i| (i & 0xFF) as u8);
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, (i & 0xFF) as u8);
        }
    }

    #[test]
    fn verify_indexed_detects_mismatch() {
        let mut buf = [0u8; 16];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        assert!(verify_indexed(buf.as_ptr(), buf.len(), |i| (i & 0xFF) as u8).is_none());
        buf[5] = 0xFF;
        assert_eq!(
            verify_indexed(buf.as_ptr(), buf.len(), |i| (i & 0xFF) as u8),
            Some(5)
        );
    }

    #[test]
    fn write_bytes_matches_fill_pattern() {
        let mut buf = [0u8; 16];
        write_bytes(buf.as_mut_ptr(), 0xDE, buf.len());
        assert!(buf.iter().all(|&b| b == 0xDE));
    }
}
