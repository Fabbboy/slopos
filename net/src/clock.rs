//! Unified monotonic clock for the networking stack.
//!
//! Every time-dependent net subsystem, the [`NetTimerWheel`](crate::timer::NetTimerWheel)
//! included, reads [`now_ms`]; there is no second tick-based time domain, so
//! one mock-clock advance fast-forwards the whole stack.

/// Abstract source of monotonic milliseconds, for tests that need a clock
/// without touching the global state; production code reads [`now_ms`].
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// The default production clock, reading the host's monotonic uptime.
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now_ms(&self) -> u64 {
        slopos_kernel_services::clock::uptime_ms()
    }
}

/// Read the current time in milliseconds.
#[cfg(not(feature = "test-hooks"))]
#[inline]
pub fn now_ms() -> u64 {
    slopos_kernel_services::clock::uptime_ms()
}

/// Read the current time in milliseconds; a zero mock value passes through to
/// real uptime, so live tests that install no mock are unaffected.
#[cfg(feature = "test-hooks")]
#[inline]
pub fn now_ms() -> u64 {
    mock::current_ms()
}

#[cfg(feature = "test-hooks")]
pub use mock::{MOCK_CLOCK, MockClock, MockClockGuard};

#[cfg(feature = "test-hooks")]
mod mock {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Mock time in milliseconds; `0` means inactive.
    pub static MOCK_CLOCK: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub fn current_ms() -> u64 {
        let m = MOCK_CLOCK.load(Ordering::Relaxed);
        if m == 0 {
            slopos_kernel_services::clock::uptime_ms()
        } else {
            m
        }
    }

    /// Test-facing handle for [`MOCK_CLOCK`]; stateless.
    pub struct MockClock;

    impl MockClock {
        /// Activate the mock clock at `v`, raised to `1` if zero, since zero
        /// means inactive.
        pub fn install_at(v: u64) {
            MOCK_CLOCK.store(v.max(1), Ordering::Relaxed);
        }

        pub fn advance(delta: u64) {
            MOCK_CLOCK.fetch_add(delta, Ordering::Relaxed);
        }

        /// Disable the mock clock; [`super::now_ms`] passes through again.
        pub fn clear() {
            MOCK_CLOCK.store(0, Ordering::Relaxed);
        }

        /// Current mock value, or `0` if inactive.
        pub fn raw() -> u64 {
            MOCK_CLOCK.load(Ordering::Relaxed)
        }
    }

    impl super::Clock for MockClock {
        fn now_ms(&self) -> u64 {
            current_ms()
        }
    }

    /// Restores the mock clock to inactive on drop, including on the early
    /// return `assert_*!` takes, which a trailing `clear()` would skip.
    ///
    /// `MOCK_CLOCK` is a process-global override of the one clock the whole net
    /// stack reads, so a leaked frozen value stops every later net timer firing,
    /// into the userland phase included. Bind it to a named local for the test
    /// body (`let _clock = …`); `let _ = …` drops it immediately.
    #[must_use = "the clock is restored when the guard drops; bind it to a named local for the test body"]
    pub struct MockClockGuard;

    impl MockClockGuard {
        /// Activate the mock clock at `v` (see [`MockClock::install_at`]).
        pub fn install_at(v: u64) -> Self {
            MockClock::install_at(v);
            MockClockGuard
        }
    }

    impl Drop for MockClockGuard {
        fn drop(&mut self) {
            MockClock::clear();
        }
    }
}
