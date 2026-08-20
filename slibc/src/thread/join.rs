use core::ffi::c_void;

use crate::errno::{EDEADLK, EINVAL};
use crate::mem::malloc;
use crate::pal::{Pal, Sys};

use super::pthread_t;
use super::tcb::Tcb;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(thread: pthread_t, retval: *mut *mut u8) -> i32 {
    if thread == 0 {
        return EINVAL.raw();
    }

    let tcb = thread as *mut Tcb;

    if (*tcb).detached {
        return EINVAL.raw();
    }

    if super::tls::tls_is_initialized() {
        let self_tcb = Tcb::current();
        if tcb == self_tcb {
            return EDEADLK.raw();
        }
    }

    loop {
        let tid_val = core::ptr::read_volatile(&(*tcb).child_tid);
        if tid_val == 0 {
            break;
        }
        super::futex::futex_wait_or_abort(
            &raw const (*tcb).child_tid as *const u32,
            tid_val as u32,
            0,
        );
    }

    if !retval.is_null() {
        *retval = (*tcb).retval;
    }

    let stack_base = (*tcb).stack_base;
    let stack_size = (*tcb).stack_size;

    if !stack_base.is_null() && stack_size > 0 {
        let _ = Sys::munmap(stack_base, stack_size);
    }

    malloc::dealloc(tcb as *mut c_void);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_detach(thread: pthread_t) -> i32 {
    if thread == 0 {
        return EINVAL.raw();
    }

    let tcb = thread as *mut Tcb;
    (*tcb).detached = true;

    let tid_val = core::ptr::read_volatile(&(*tcb).child_tid);
    if tid_val == 0 {
        let stack_base = (*tcb).stack_base;
        let stack_size = (*tcb).stack_size;
        if !stack_base.is_null() && stack_size > 0 {
            let _ = Sys::munmap(stack_base, stack_size);
        }
        malloc::dealloc(tcb as *mut c_void);
    }

    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(retval: *mut u8) -> ! {
    if super::tls::tls_is_initialized() {
        let tcb = Tcb::current();
        (*tcb).retval = retval;
        super::keys::run_key_destructors(tcb);
    }
    crate::process::_exit(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_self() -> pthread_t {
    if super::tls::tls_is_initialized() {
        Tcb::current() as pthread_t
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_equal(t1: pthread_t, t2: pthread_t) -> i32 {
    if t1 == t2 { 1 } else { 0 }
}
