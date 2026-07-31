//! Per-stream recursive lock.
//!
//! POSIX §2.5.1 requires every stdio function to behave as if it acquired the
//! stream's lock for the duration of the call, and requires `flockfile` to be
//! recursive so a thread that already owns a stream may lock it again. That
//! recursion is not a convenience: `printf` takes the stream once per call and
//! then emits through the same primitives a caller could have taken it with.
//!
//! `pthread_mutex_t` is a bare futex word, so the owner identity and the
//! recursion count live here. Identity is the thread's TCB address — one
//! `mov rax, fs:[0]`, no syscall. Before TLS is installed exactly one thread
//! exists, so a constant stands in for it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::thread::mutex::{
    PTHREAD_MUTEX_INITIALIZER, pthread_mutex_lock, pthread_mutex_t, pthread_mutex_trylock,
    pthread_mutex_unlock,
};
use crate::thread::tcb::Tcb;
use crate::thread::tls::tls_is_initialized;

/// Stand-in owner identity for the window before `fs_base` is installed.
/// Never collides with a TCB address.
const PRE_TLS_THREAD: usize = 1;

/// Identity of the calling thread, for the recursive-ownership test.
fn thread_identity() -> usize {
    if tls_is_initialized() {
        // SAFETY: `tls_is_initialized` reports that `fs_base` holds a live TCB.
        unsafe { Tcb::current() as usize }
    } else {
        PRE_TLS_THREAD
    }
}

/// A recursive mutex guarding one `FILE`.
#[repr(C)]
pub struct StreamLock {
    mutex: UnsafeCell<pthread_mutex_t>,
    owner: AtomicUsize,
    depth: AtomicU32,
}

impl StreamLock {
    pub const fn new() -> StreamLock {
        StreamLock {
            mutex: UnsafeCell::new(PTHREAD_MUTEX_INITIALIZER),
            owner: AtomicUsize::new(0),
            depth: AtomicU32::new(0),
        }
    }

    /// Take one more level if `me` already owns the lock. Only the owner ever
    /// writes `depth`, so the read-modify-write needs no atomicity.
    fn reenter(&self, me: usize) -> bool {
        if self.owner.load(Ordering::Acquire) != me {
            return false;
        }
        self.depth
            .store(self.depth.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        true
    }

    /// Record the first acquisition, with the futex already held.
    fn claim(&self, me: usize) {
        self.owner.store(me, Ordering::Release);
        self.depth.store(1, Ordering::Relaxed);
    }

    /// Acquire the lock, blocking. Re-entrant for the owning thread.
    pub fn lock(&self) {
        let me = thread_identity();
        if self.reenter(me) {
            return;
        }
        // SAFETY: `mutex` is a live futex word owned by this lock.
        unsafe {
            pthread_mutex_lock(self.mutex.get());
        }
        self.claim(me);
    }

    /// Acquire the lock without blocking. Returns `true` if it is now held by
    /// the caller — including the re-entrant case.
    pub fn try_lock(&self) -> bool {
        let me = thread_identity();
        if self.reenter(me) {
            return true;
        }
        // SAFETY: as in `lock`.
        if unsafe { pthread_mutex_trylock(self.mutex.get()) } != 0 {
            return false;
        }
        self.claim(me);
        true
    }

    /// Release one level of ownership.
    pub fn unlock(&self) {
        let depth = self.depth.load(Ordering::Relaxed);
        if depth > 1 {
            self.depth.store(depth - 1, Ordering::Relaxed);
            return;
        }
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Release);
        // SAFETY: as in `lock`; the caller owns the lock.
        unsafe {
            pthread_mutex_unlock(self.mutex.get());
        }
    }
}
