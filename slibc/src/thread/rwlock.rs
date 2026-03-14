#![allow(non_camel_case_types)]

use core::sync::atomic::{AtomicI32, Ordering};

use crate::errno::{EBUSY, EINVAL};
use crate::pal::{Pal, Sys};

#[repr(C)]
pub struct pthread_rwlock_t {
    pub state: AtomicI32,
    pub writer_waiting: AtomicI32,
}

#[repr(C)]
pub struct pthread_rwlockattr_t {
    _unused: i32,
}

pub const PTHREAD_RWLOCK_INITIALIZER: pthread_rwlock_t = pthread_rwlock_t {
    state: AtomicI32::new(0),
    writer_waiting: AtomicI32::new(0),
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_init(
    rwlock: *mut pthread_rwlock_t,
    _attr: *const pthread_rwlockattr_t,
) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    (*rwlock).state = AtomicI32::new(0);
    (*rwlock).writer_waiting = AtomicI32::new(0);
    0
}

/// state > 0: N readers, state == 0: unlocked, state == -1: writer holds lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_rdlock(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    loop {
        let s = (*rwlock).state.load(Ordering::Acquire);
        if s >= 0 && (*rwlock).writer_waiting.load(Ordering::Acquire) == 0 {
            if (*rwlock)
                .state
                .compare_exchange(s, s + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return 0;
            }
        } else {
            let _ = Sys::futex_wait((*rwlock).state.as_ptr() as *const u32, s as u32, 0);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_tryrdlock(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    let s = (*rwlock).state.load(Ordering::Acquire);
    if s >= 0 && (*rwlock).writer_waiting.load(Ordering::Acquire) == 0 {
        if (*rwlock)
            .state
            .compare_exchange(s, s + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return 0;
        }
    }
    EBUSY.raw()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_wrlock(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    (*rwlock).writer_waiting.fetch_add(1, Ordering::Release);
    loop {
        if (*rwlock)
            .state
            .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            (*rwlock).writer_waiting.fetch_sub(1, Ordering::Release);
            return 0;
        }
        let s = (*rwlock).state.load(Ordering::Relaxed);
        let _ = Sys::futex_wait((*rwlock).state.as_ptr() as *const u32, s as u32, 0);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_trywrlock(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    if (*rwlock)
        .state
        .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        0
    } else {
        EBUSY.raw()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_unlock(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }

    let prev = (*rwlock).state.load(Ordering::Acquire);
    if prev == -1 {
        (*rwlock).state.store(0, Ordering::Release);
        let _ = Sys::futex_wake((*rwlock).state.as_ptr() as *const u32, i32::MAX as u32);
    } else if prev > 0 {
        let new_val = (*rwlock).state.fetch_sub(1, Ordering::Release) - 1;
        if new_val == 0 {
            let _ = Sys::futex_wake((*rwlock).state.as_ptr() as *const u32, 1);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_destroy(rwlock: *mut pthread_rwlock_t) -> i32 {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    if (*rwlock).state.load(Ordering::Relaxed) != 0 {
        return EBUSY.raw();
    }
    (*rwlock).state = AtomicI32::new(0);
    (*rwlock).writer_waiting = AtomicI32::new(0);
    0
}
