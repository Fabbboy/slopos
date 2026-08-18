//! Byte-level helpers for the HHDM (kernel direct-map) window.
//!
//! Each helper folds exactly one `core::ptr` byte operation interior to OSTD so
//! the consumer call sites stay safe, and bounds-checks the `(virt, offset,
//! len)` triple against `PAGE_SIZE_4KB` here rather than at the caller.
//!
//! # Safety contract on every helper
//!
//! Each function is **safe to call**. The interior `unsafe` is sound
//! whenever the caller ensures:
//!
//! - `virt` is non-null and resolves (via the live HHDM mapping) to a
//!   physical frame the caller has just pinned for the duration of the
//!   call (typically by holding the `VmSpace` lock or the per-process
//!   slot lock that owns the frame).
//! - `offset + len <= PAGE_SIZE_4KB` (re-checked here for the byte
//!   helpers; the unaligned read/write helpers leave size-typing to
//!   `T`).
//! - For multi-page operations the caller has separately verified
//!   buddy contiguity (callers in the ELF loader do this).

use slopos_abi::addr::VirtAddr;

/// Duplicated from `slopos_mm::paging_defs::PAGE_SIZE_4KB` so this module has
/// no dependency on the `slopos-mm` paging definitions.
const PAGE_SIZE_4KB: usize = 0x1000;

/// Copy `src.len()` bytes through the HHDM mapping at `virt + offset`.
/// Returns `false` if `virt` is null or `offset + src.len() > 4096`.
#[inline]
pub fn write_bytes(virt: VirtAddr, offset: usize, src: &[u8]) -> bool {
    if virt.is_null() {
        return false;
    }
    if offset
        .checked_add(src.len())
        .is_none_or(|e| e > PAGE_SIZE_4KB)
    {
        return false;
    }
    // SAFETY: module contract — `virt` resolves to a live HHDM mapping;
    // bounds-checked against the page above.
    unsafe {
        core::ptr::copy_nonoverlapping(
            src.as_ptr(),
            virt.as_mut_ptr::<u8>().add(offset),
            src.len(),
        );
    }
    true
}

/// Read `dst.len()` bytes from the HHDM mapping at `virt + offset` into `dst`.
/// Same caller contract as [`write_bytes`].
#[inline]
pub fn read_bytes(virt: VirtAddr, offset: usize, dst: &mut [u8]) -> bool {
    if virt.is_null() {
        return false;
    }
    if offset
        .checked_add(dst.len())
        .is_none_or(|e| e > PAGE_SIZE_4KB)
    {
        return false;
    }
    // SAFETY: as `write_bytes` — read direction is symmetric.
    unsafe {
        core::ptr::copy_nonoverlapping(
            virt.as_ptr::<u8>().add(offset),
            dst.as_mut_ptr(),
            dst.len(),
        );
    }
    true
}

/// Fill `len` bytes at the HHDM mapping at `virt + offset` with `value`.
/// Same caller contract as [`write_bytes`]. Returns `false` if `virt` is
/// null, `len == 0`, or the bounds check fails.
#[inline]
pub fn fill_bytes(virt: VirtAddr, offset: usize, len: usize, value: u8) -> bool {
    if virt.is_null() || len == 0 {
        return false;
    }
    if offset.checked_add(len).is_none_or(|e| e > PAGE_SIZE_4KB) {
        return false;
    }
    // SAFETY: as `write_bytes`.
    unsafe {
        core::ptr::write_bytes(virt.as_mut_ptr::<u8>().add(offset), value, len);
    }
    true
}

/// Read an unaligned `T: Copy` at `virt + offset` through the HHDM mapping.
/// Returns `None` if `virt` is null. `T: Copy` because the underlying bytes may
/// hold any pattern (a relocation site, a page about to be filled).
#[inline]
pub fn read_unaligned<T: Copy>(virt: VirtAddr, offset: usize) -> Option<T> {
    if virt.is_null() {
        return None;
    }
    // SAFETY: caller-pinned HHDM mapping; `T: Copy` accepts any byte
    // pattern; `read_unaligned` lifts the alignment requirement.
    let p = unsafe { virt.as_ptr::<u8>().add(offset) } as *const T;
    Some(unsafe { core::ptr::read_unaligned(p) })
}

/// Write an unaligned `T: Copy` at `virt + offset` through the HHDM mapping.
/// Returns `false` if `virt` is null.
#[inline]
pub fn write_unaligned<T: Copy>(virt: VirtAddr, offset: usize, value: T) -> bool {
    if virt.is_null() {
        return false;
    }
    // SAFETY: caller-pinned HHDM mapping; `write_unaligned` lifts the
    // alignment requirement.
    let p = unsafe { virt.as_mut_ptr::<u8>().add(offset) } as *mut T;
    unsafe { core::ptr::write_unaligned(p, value) };
    true
}

/// Copy a full 4 KiB page from one HHDM-mapped virt to another. Returns `false`
/// if either pointer is null. Caller must guarantee the two mappings address
/// distinct underlying frames.
#[inline]
pub fn copy_page(src: VirtAddr, dst: VirtAddr) -> bool {
    if src.is_null() || dst.is_null() {
        return false;
    }
    // SAFETY: caller has pinned both HHDM mappings and guarantees they do not
    // alias.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr::<u8>(), dst.as_mut_ptr::<u8>(), PAGE_SIZE_4KB);
    }
    true
}
