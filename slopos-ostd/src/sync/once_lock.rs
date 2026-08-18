//! Thread-safe lazy initialization container.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const STATE_UNINIT: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_COMPLETE: u8 = 2;

/// A thread-safe container for one-time initialization.
pub struct OnceLock<T> {
    state: AtomicU8,
    data: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: OnceLock ensures exclusive write access during initialization
// through atomic state transitions (only one thread can CAS UNINIT→RUNNING),
// and shared read access thereafter (state == COMPLETE is immutable).
unsafe impl<T: Send + Sync> Send for OnceLock<T> {}
unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}

impl<T> OnceLock<T> {
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_UNINIT),
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Concurrent callers spin (with `PAUSE`) until the first caller's closure
    /// completes; later callers are no-ops and never invoke the closure.
    #[inline]
    pub fn call_once(&self, f: impl FnOnce() -> T) {
        if self.state.load(Ordering::Acquire) == STATE_COMPLETE {
            return;
        }

        if self
            .state
            .compare_exchange(
                STATE_UNINIT,
                STATE_RUNNING,
                Ordering::Acquire,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let value = f();
            // SAFETY: we are the sole writer (STATE_RUNNING guarantees exclusivity).
            unsafe { (*self.data.get()).write(value) };
            self.state.store(STATE_COMPLETE, Ordering::Release);
        } else {
            while self.state.load(Ordering::Acquire) != STATE_COMPLETE {
                core::hint::spin_loop();
            }
        }
    }

    #[inline]
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == STATE_COMPLETE {
            // SAFETY: state == COMPLETE guarantees the value was fully written
            // with Release ordering, and our Acquire load synchronizes with it.
            Some(unsafe { (*self.data.get()).assume_init_ref() })
        } else {
            None
        }
    }

    #[inline]
    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_COMPLETE
    }

    /// An unbounded spin with `PAUSE` hints: only for pre-scheduler rendezvous
    /// points where parking the task is impossible.
    #[inline]
    pub fn wait(&self) -> &T {
        while self.state.load(Ordering::Acquire) != STATE_COMPLETE {
            core::hint::spin_loop();
        }
        // SAFETY: state == COMPLETE guarantees the value was fully written
        // with Release ordering before our Acquire load above.
        unsafe { (*self.data.get()).assume_init_ref() }
    }
}

impl<T> Default for OnceLock<T> {
    fn default() -> Self {
        Self::new()
    }
}
