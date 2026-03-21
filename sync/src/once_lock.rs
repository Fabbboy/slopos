//! Thread-safe lazy initialization container.
//!
//! [`OnceLock<T>`] provides one-time initialization with [`call_once()`] and
//! subsequent access via [`get()`].  The first caller to `call_once()` runs
//! the initializer; concurrent callers spin until complete; later callers
//! are no-ops.
//!
//! This replaces the external `spin::Once` crate with a kernel-native
//! implementation that is consistent with the ticket-lock-based locking
//! strategy used throughout SlopOS.
//!
//! [`call_once()`]: OnceLock::call_once
//! [`get()`]: OnceLock::get

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const STATE_UNINIT: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_COMPLETE: u8 = 2;

/// A thread-safe container for one-time initialization.
///
/// The value is lazily initialized on the first call to [`call_once()`].
/// Subsequent calls are no-ops.  [`get()`] returns `Some(&T)` once
/// initialization is complete.
///
/// [`call_once()`]: OnceLock::call_once
/// [`get()`]: OnceLock::get
///
/// # Example
///
/// ```ignore
/// use slopos_sync::OnceLock;
///
/// static CONFIG: OnceLock<Config> = OnceLock::new();
///
/// fn init() {
///     CONFIG.call_once(|| Config::default());
/// }
///
/// fn use_config() -> &'static Config {
///     CONFIG.get().expect("CONFIG not initialized")
/// }
/// ```
pub struct OnceLock<T> {
    /// 0 = uninit, 1 = initializer running, 2 = complete.
    state: AtomicU8,
    data: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: OnceLock ensures exclusive write access during initialization
// through atomic state transitions (only one thread can CAS UNINIT→RUNNING),
// and shared read access thereafter (state == COMPLETE is immutable).
unsafe impl<T: Send + Sync> Send for OnceLock<T> {}
unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}

impl<T> OnceLock<T> {
    /// Create a new uninitialized `OnceLock`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_UNINIT),
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Initialize the value if not yet initialized.
    ///
    /// The first caller's closure runs to completion and stores the result.
    /// Concurrent callers spin (with `PAUSE`) until initialization completes.
    /// Subsequent callers are no-ops — the closure is never invoked.
    #[inline]
    pub fn call_once(&self, f: impl FnOnce() -> T) {
        // Fast path: already initialized.
        if self.state.load(Ordering::Acquire) == STATE_COMPLETE {
            return;
        }

        // Try to claim the initializer role.
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
            // We won the race: run the initializer.
            let value = f();
            // SAFETY: we are the sole writer (STATE_RUNNING guarantees exclusivity).
            unsafe { (*self.data.get()).write(value) };
            // Publish the value to all CPUs.
            self.state.store(STATE_COMPLETE, Ordering::Release);
        } else {
            // Someone else is initializing — spin until complete.
            while self.state.load(Ordering::Acquire) != STATE_COMPLETE {
                core::hint::spin_loop();
            }
        }
    }

    /// Returns a reference to the value if initialized, or `None`.
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

    /// Returns `true` if the value has been initialized.
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_COMPLETE
    }
}
