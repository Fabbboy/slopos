//! Keyboard-layout syscalls.

use super::raw::syscall2;
use slopos_abi::syscall::{SYSCALL_KEYMAP_GET_NAME, SYSCALL_KEYMAP_LOAD};

/// Install a serialised `LayoutTable` blob (see `slopos_keymap_core::serialize`);
/// 0 on success, negative errno otherwise. Unprivileged — the kernel-side
/// binary validator is the safety boundary.
pub fn keymap_load(blob: &[u8]) -> i64 {
    unsafe { syscall2(SYSCALL_KEYMAP_LOAD, blob.as_ptr() as u64, blob.len() as u64) as i64 }
}

/// Copies the active layout's short name into `buf`; bytes written, or errno.
pub fn keymap_get_name(buf: &mut [u8]) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_KEYMAP_GET_NAME,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
}
