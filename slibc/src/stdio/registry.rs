//! The open-stream list.
//!
//! C11 §7.21.5.2 (`fflush(NULL)`) and §7.22.4.4 (`exit`) both have to name
//! every open stream, so every `FILE` is on this list, the three standard
//! streams included.
//!
//! **Two lock levels, and the list outranks the stream.** The walk takes the
//! list lock and then each stream's lock in turn; nothing takes the list lock
//! while holding a stream lock, so no cycle is expressible. The walk keeps the
//! list lock across blocking `write()` syscalls, and `fclose` unlinks *before*
//! it flushes, so no `FILE` can be freed mid-flush.

use core::ptr;

use crate::pal::{Pal, Sys};
use crate::thread::mutex::{
    PTHREAD_MUTEX_INITIALIZER, pthread_mutex_lock, pthread_mutex_t, pthread_mutex_unlock,
};

use super::{EOF, FILE, FILE_FLAG_LINKED, FILE_FLAG_WRITING};

/// How a walk treats a stream lock it cannot take immediately.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WalkMode {
    /// Wait for every lock. A live program must not silently skip a stream.
    Blocking,
    /// Bounded try, then move on. Used at exit, where a peer thread wedged in
    /// `write()` must not turn a clean termination into a hang.
    BestEffort,
}

/// Attempts, one millisecond apart, before a `BestEffort` walk gives up on a
/// stream.
const BEST_EFFORT_ATTEMPTS: u32 = 16;

static mut OPEN_STREAMS: *mut FILE = ptr::null_mut();
static mut LIST_LOCK: pthread_mutex_t = PTHREAD_MUTEX_INITIALIZER;

fn list_lock() {
    // SAFETY: `LIST_LOCK` is a process-wide futex word with static lifetime.
    unsafe {
        pthread_mutex_lock(&raw mut LIST_LOCK);
    }
}

fn list_unlock() {
    unsafe {
        pthread_mutex_unlock(&raw mut LIST_LOCK);
    }
}

/// Add `stream` to the open-stream list. Idempotent — re-linking an already
/// linked node would make the walk spin on `f.next == f`.
///
/// # Safety
/// `stream` must point at a live `FILE` that outlives its membership.
pub unsafe fn link(stream: *mut FILE) {
    if stream.is_null() {
        return;
    }
    list_lock();
    let f = &mut *stream;
    if f.flags & FILE_FLAG_LINKED == 0 {
        f.flags |= FILE_FLAG_LINKED;
        f.next = OPEN_STREAMS;
        OPEN_STREAMS = stream;
    }
    list_unlock();
}

/// # Safety
/// `stream` must point at a live `FILE`.
pub unsafe fn unlink(stream: *mut FILE) {
    if stream.is_null() {
        return;
    }
    list_lock();
    let mut cursor = &raw mut OPEN_STREAMS;
    while !(*cursor).is_null() {
        let node = *cursor;
        if node == stream {
            *cursor = (*node).next;
            (*node).next = ptr::null_mut();
            (*node).flags &= !FILE_FLAG_LINKED;
            break;
        }
        cursor = &raw mut (*node).next;
    }
    list_unlock();
}

/// Take `stream`'s lock according to `mode`. Returns `false` only when a
/// `BestEffort` walk gave up.
fn acquire(stream: &FILE, mode: WalkMode) -> bool {
    match mode {
        WalkMode::Blocking => {
            stream.lock.lock();
            true
        }
        WalkMode::BestEffort => {
            for _ in 0..BEST_EFFORT_ATTEMPTS {
                if stream.lock.try_lock() {
                    return true;
                }
                Sys::sleep_ms(1);
            }
            false
        }
    }
}

/// Flush every stream whose most recent operation was output. Read-direction
/// streams are left alone: C11 §7.21.5.2 scopes `fflush(NULL)` to output, and
/// dropping a pipe's read-ahead would destroy unconsumed data. Returns 0, or
/// [`EOF`] if any stream reported a write error.
pub fn flush_all(mode: WalkMode) -> i32 {
    let mut ret = 0i32;
    list_lock();
    // SAFETY: the list lock is held, so every reachable node is live — an
    // `fclose` racing this walk blocks on the same lock before it frees.
    unsafe {
        let mut node = OPEN_STREAMS;
        while !node.is_null() {
            let f = &mut *node;
            let next = f.next;
            if acquire(f, mode) {
                if f.flags & FILE_FLAG_WRITING != 0 && f.flush_write_buf() == EOF {
                    ret = EOF;
                }
                f.lock.unlock();
            } else {
                ret = EOF;
            }
            node = next;
        }
    }
    list_unlock();
    ret
}
