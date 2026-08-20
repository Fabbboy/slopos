#![allow(non_camel_case_types)]

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::errno::EINVAL;
use crate::pal::{Pal, Sys};

use super::mutex::{pthread_mutex_lock, pthread_mutex_t, pthread_mutex_unlock};

#[repr(C)]
pub struct pthread_cond_t {
    pub seq: AtomicU32,
    pub mutex: *mut pthread_mutex_t,
}

unsafe impl Send for pthread_cond_t {}
unsafe impl Sync for pthread_cond_t {}

#[repr(C)]
pub struct pthread_condattr_t {
    _unused: i32,
}

pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t {
    seq: AtomicU32::new(0),
    mutex: ptr::null_mut(),
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_init(
    cond: *mut pthread_cond_t,
    _attr: *const pthread_condattr_t,
) -> i32 {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq = AtomicU32::new(0);
    (*cond).mutex = ptr::null_mut();
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> i32 {
    if cond.is_null() || mutex.is_null() {
        return EINVAL.raw();
    }

    let saved_seq = (*cond).seq.load(Ordering::Relaxed);
    (*cond).mutex = mutex;

    pthread_mutex_unlock(mutex);
    super::futex::futex_wait_or_abort((*cond).seq.as_ptr() as *const u32, saved_seq, 0);
    pthread_mutex_lock(mutex);

    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> i32 {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq.fetch_add(1, Ordering::Release);
    let _ = Sys::futex_wake((*cond).seq.as_ptr() as *const u32, 1);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> i32 {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq.fetch_add(1, Ordering::Release);
    let _ = Sys::futex_wake((*cond).seq.as_ptr() as *const u32, i32::MAX as u32);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> i32 {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq = AtomicU32::new(0);
    (*cond).mutex = ptr::null_mut();
    0
}
