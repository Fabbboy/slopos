#![allow(non_camel_case_types)]

use core::sync::atomic::{AtomicI32, Ordering};

use crate::errno::{EBUSY, EINVAL};
use crate::pal::{Pal, Sys};

pub const PTHREAD_MUTEX_NORMAL: i32 = 0;
pub const PTHREAD_MUTEX_RECURSIVE: i32 = 1;
pub const PTHREAD_MUTEX_ERRORCHECK: i32 = 2;

#[repr(C)]
pub struct pthread_mutex_t {
    pub state: AtomicI32,
    pub owner_tid: i32,
    pub kind: i32,
    pub count: i32,
}

#[repr(C)]
pub struct pthread_mutexattr_t {
    pub kind: i32,
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t {
    state: AtomicI32::new(0),
    owner_tid: 0,
    kind: PTHREAD_MUTEX_NORMAL,
    count: 0,
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_init(
    mutex: *mut pthread_mutex_t,
    attr: *const pthread_mutexattr_t,
) -> i32 {
    if mutex.is_null() {
        return EINVAL.raw();
    }
    (*mutex).state = AtomicI32::new(0);
    (*mutex).owner_tid = 0;
    (*mutex).count = 0;
    (*mutex).kind = if attr.is_null() {
        PTHREAD_MUTEX_NORMAL
    } else {
        (*attr).kind
    };
    0
}

/// Futex-based lock: 0=unlocked, 1=locked, 2=locked+waiters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> i32 {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    let state = &(*mutex).state;

    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return 0;
    }

    loop {
        let old = state.swap(2, Ordering::Acquire);
        if old == 0 {
            return 0;
        }
        let _ = Sys::futex_wait(state.as_ptr() as *const u32, 2, 0);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> i32 {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    let state = &(*mutex).state;
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        0
    } else {
        EBUSY.raw()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> i32 {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    let state = &(*mutex).state;
    if state.swap(0, Ordering::Release) == 2 {
        let _ = Sys::futex_wake(state.as_ptr() as *const u32, 1);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> i32 {
    if mutex.is_null() {
        return EINVAL.raw();
    }
    if (*mutex).state.load(Ordering::Relaxed) != 0 {
        return EBUSY.raw();
    }
    (*mutex).state = AtomicI32::new(0);
    (*mutex).owner_tid = 0;
    (*mutex).kind = 0;
    (*mutex).count = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_init(attr: *mut pthread_mutexattr_t) -> i32 {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).kind = PTHREAD_MUTEX_NORMAL;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_settype(
    attr: *mut pthread_mutexattr_t,
    kind: i32,
) -> i32 {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).kind = kind;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_destroy(attr: *mut pthread_mutexattr_t) -> i32 {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).kind = 0;
    0
}
