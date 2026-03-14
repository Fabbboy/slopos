//! Futex implementation for SlopOS.
//!
//! Wraps the SlopOS futex syscalls (FUTEX_WAIT / FUTEX_WAKE) for use by
//! std's sync primitives (Mutex, Condvar, RwLock, Once, Parker).

use crate::sync::atomic::Atomic;
use crate::time::Duration;

/// An atomic for use as a futex that is at least 32-bits but may be larger.
pub type Futex = Atomic<Primitive>;
/// Must be the underlying type of Futex.
pub type Primitive = u32;

/// An atomic for use as a futex that is at least 8-bits but may be larger.
pub type SmallFutex = Atomic<SmallPrimitive>;
/// Must be the underlying type of SmallFutex.
pub type SmallPrimitive = u32;

unsafe extern "C" {
    // SlopOS futex syscall wrappers from slibc:
    //   slopos_futex_wait(addr, expected, timeout_ms) -> i32
    //     returns 0 on wake, -ETIMEDOUT on timeout, -errno on error
    //   slopos_futex_wake(addr, count) -> i32
    //     returns number of threads woken, or -errno on error
    fn slopos_futex_wait(addr: *const u32, expected: u32, timeout_ms: u64) -> i32;
    fn slopos_futex_wake(addr: *const u32, count: u32) -> i32;
}

/// Wait on a futex. Returns `true` if woken (or spurious), `false` if timed out.
pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let timeout_ms = match timeout {
        Some(dur) => {
            let ms = dur.as_millis();
            if ms > u64::MAX as u128 {
                u64::MAX
            } else if ms == 0 && !dur.is_zero() {
                // Sub-millisecond timeout: round up to 1ms
                1u64
            } else {
                ms as u64
            }
        }
        // 0 means infinite wait in SlopOS
        None => 0,
    };

    let ret = unsafe { slopos_futex_wait(futex.as_ptr(), expected, timeout_ms) };
    // ret == 0: woken normally
    // ret == -110 (ETIMEDOUT): timed out
    // ret < 0: some error (treat as spurious wake)
    ret != -110
}

/// Wake one thread waiting on this futex. Returns `true` if a thread was woken.
#[inline]
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    unsafe { slopos_futex_wake(futex.as_ptr(), 1) > 0 }
}

/// Wake all threads waiting on this futex.
#[inline]
pub fn futex_wake_all(futex: &Atomic<u32>) {
    unsafe {
        slopos_futex_wake(futex.as_ptr(), i32::MAX as u32);
    }
}
