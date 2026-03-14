#![allow(non_camel_case_types)]

pub mod condvar;
pub mod create;
pub mod join;
pub mod keys;
pub mod mutex;
pub mod rwlock;
pub mod tcb;
pub mod tests;
pub mod tls;

pub type pthread_t = u64;

#[repr(C)]
pub struct pthread_attr_t {
    pub stack_size: usize,
    pub detach_state: i32,
}

pub const PTHREAD_CREATE_JOINABLE: i32 = 0;
pub const PTHREAD_CREATE_DETACHED: i32 = 1;
pub const DEFAULT_STACK_SIZE: usize = 2 * 1024 * 1024;
pub const PTHREAD_STACK_MIN: usize = 16384;

pub use condvar::{PTHREAD_COND_INITIALIZER, pthread_cond_t};
pub use create::pthread_create;
pub use join::{pthread_detach, pthread_equal, pthread_exit, pthread_join, pthread_self};
pub use keys::pthread_key_t;
pub use mutex::{
    PTHREAD_MUTEX_ERRORCHECK, PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_NORMAL,
    PTHREAD_MUTEX_RECURSIVE, pthread_mutex_t,
};
pub use rwlock::{PTHREAD_RWLOCK_INITIALIZER, pthread_rwlock_t};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_init(attr: *mut pthread_attr_t) -> i32 {
    if attr.is_null() {
        return crate::errno::EINVAL.raw();
    }
    (*attr).stack_size = DEFAULT_STACK_SIZE;
    (*attr).detach_state = PTHREAD_CREATE_JOINABLE;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_destroy(attr: *mut pthread_attr_t) -> i32 {
    if attr.is_null() {
        return crate::errno::EINVAL.raw();
    }
    (*attr).stack_size = 0;
    (*attr).detach_state = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setstacksize(
    attr: *mut pthread_attr_t,
    stacksize: usize,
) -> i32 {
    if attr.is_null() || stacksize < PTHREAD_STACK_MIN {
        return crate::errno::EINVAL.raw();
    }
    (*attr).stack_size = stacksize;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getstacksize(
    attr: *const pthread_attr_t,
    stacksize: *mut usize,
) -> i32 {
    if attr.is_null() || stacksize.is_null() {
        return crate::errno::EINVAL.raw();
    }
    *stacksize = (*attr).stack_size;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setdetachstate(
    attr: *mut pthread_attr_t,
    detachstate: i32,
) -> i32 {
    if attr.is_null() {
        return crate::errno::EINVAL.raw();
    }
    if detachstate != PTHREAD_CREATE_JOINABLE && detachstate != PTHREAD_CREATE_DETACHED {
        return crate::errno::EINVAL.raw();
    }
    (*attr).detach_state = detachstate;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getdetachstate(
    attr: *const pthread_attr_t,
    detachstate: *mut i32,
) -> i32 {
    if attr.is_null() || detachstate.is_null() {
        return crate::errno::EINVAL.raw();
    }
    *detachstate = (*attr).detach_state;
    0
}
