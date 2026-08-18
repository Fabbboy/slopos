//! Keyboard-layout service interface. The active layout lives in the keyboard
//! driver, but `core` owns the syscall table and cannot depend on `drivers`, so
//! `drivers` registers the implementation and `core` calls the wrappers.

use slopos_abi::Errno;

slopos_service_core::define_service! {
    keymap => KeymapServices {
        /// Install a serialised `LayoutTable` blob at `data_ptr`/`len` (read
        /// from the calling task's user memory). Returns `EINVAL` if malformed.
        load(data_ptr: u64, len: usize) -> Result<(), Errno>;
        /// Write the active layout's short name into the kernel buffer `out`
        /// (the syscall handler does the SMAP-safe copy out); returns the
        /// number of bytes written.
        current_name(out: &mut [u8]) -> usize;
    }
}
