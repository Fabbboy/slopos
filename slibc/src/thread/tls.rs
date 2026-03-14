//! TLS initialization — heap-allocate and install TCB via FS_BASE.

use core::mem;
use core::ptr;

use crate::mem::malloc;
use crate::pal::{Pal, Sys};

use super::tcb::Tcb;

static mut TLS_READY: bool = false;

#[inline]
pub fn tls_is_initialized() -> bool {
    unsafe { TLS_READY }
}

/// # Safety
/// Must be called exactly once from the main thread during CRT startup.
pub unsafe fn tls_init_main_thread() {
    let tcb_ptr = malloc::alloc(mem::size_of::<Tcb>()) as *mut Tcb;
    if tcb_ptr.is_null() {
        return;
    }

    ptr::write_bytes(tcb_ptr, 0, 1);
    (*tcb_ptr).self_ptr = tcb_ptr;
    (*tcb_ptr).tid = Sys::getpid();

    if Sys::arch_prctl_set_fs(tcb_ptr as u64).is_err() {
        malloc::dealloc(tcb_ptr as *mut core::ffi::c_void);
        return;
    }

    TLS_READY = true;
}

/// # Safety
/// `tcb` must be a valid TCB pointer passed as TLS arg to `clone()`.
pub unsafe fn tls_init_new_thread(tcb: *mut Tcb) {
    debug_assert_eq!((*tcb).self_ptr, tcb);
}
