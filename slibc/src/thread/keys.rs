use core::ptr;

use crate::errno::EINVAL;

use super::tcb::{PTHREAD_KEYS_MAX, Tcb};

#[allow(non_camel_case_types)]
pub type pthread_key_t = u32;

static mut KEY_DESTRUCTORS: [Option<unsafe extern "C" fn(*mut u8)>; PTHREAD_KEYS_MAX] =
    [None; PTHREAD_KEYS_MAX];
static mut KEY_USED: [bool; PTHREAD_KEYS_MAX] = [false; PTHREAD_KEYS_MAX];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_key_create(
    key: *mut pthread_key_t,
    destructor: Option<unsafe extern "C" fn(*mut u8)>,
) -> i32 {
    if key.is_null() {
        return EINVAL.raw();
    }

    for i in 0..PTHREAD_KEYS_MAX {
        if !KEY_USED[i] {
            KEY_USED[i] = true;
            KEY_DESTRUCTORS[i] = destructor;
            *key = i as pthread_key_t;
            return 0;
        }
    }

    crate::errno::EAGAIN.raw()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_key_delete(key: pthread_key_t) -> i32 {
    let idx = key as usize;
    if idx >= PTHREAD_KEYS_MAX || !KEY_USED[idx] {
        return EINVAL.raw();
    }
    KEY_USED[idx] = false;
    KEY_DESTRUCTORS[idx] = None;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_getspecific(key: pthread_key_t) -> *mut u8 {
    let idx = key as usize;
    if idx >= PTHREAD_KEYS_MAX || !KEY_USED[idx] {
        return ptr::null_mut();
    }
    if !super::tls::tls_is_initialized() {
        return ptr::null_mut();
    }
    let tcb = Tcb::current();
    (*tcb).thread_local_keys[idx]
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_setspecific(key: pthread_key_t, value: *mut u8) -> i32 {
    let idx = key as usize;
    if idx >= PTHREAD_KEYS_MAX || !KEY_USED[idx] {
        return EINVAL.raw();
    }
    if !super::tls::tls_is_initialized() {
        return EINVAL.raw();
    }
    let tcb = Tcb::current();
    (*tcb).thread_local_keys[idx] = value;
    0
}

/// Run destructors for all non-null key values. Called from `pthread_exit`
/// and `thread_trampoline` before thread termination.
///
/// # Safety
/// `tcb` must be valid and belong to the exiting thread.
pub unsafe fn run_key_destructors(tcb: *mut Tcb) {
    for i in 0..PTHREAD_KEYS_MAX {
        if KEY_USED[i] {
            let val = (*tcb).thread_local_keys[i];
            if !val.is_null() {
                (*tcb).thread_local_keys[i] = ptr::null_mut();
                if let Some(dtor) = KEY_DESTRUCTORS[i] {
                    dtor(val);
                }
            }
        }
    }
}
