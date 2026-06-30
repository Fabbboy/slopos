//! Keyboard-layout service interface.
//!
//! The active keyboard layout lives in the keyboard driver (which owns the
//! decode state and runs the IRQ-context resolver), but the `core` crate owns
//! the syscall table and cannot depend on `drivers`. This service bridges the
//! two: `drivers` registers the implementation, `core`'s `keymap_load` /
//! `keymap_get_name` handlers call the generated wrappers.

use slopos_abi::Errno;

slopos_service_core::define_service! {
    keymap => KeymapServices {
        /// Install a serialised `LayoutTable` blob at `data_ptr`/`len` (read
        /// from the calling task's user memory). Returns `EINVAL` if malformed.
        load(data_ptr: u64, len: usize) -> Result<(), Errno>;
        /// Write the active layout's short name into the kernel buffer `out`
        /// (the syscall handler copies it out to user memory SMAP-safely);
        /// returns the number of bytes written.
        current_name(out: &mut [u8]) -> usize;
    }
}
