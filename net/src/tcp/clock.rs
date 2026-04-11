//! Monotonic clock abstraction used by the TCP state machine.
//!
//! Production code reads `now_ms()` which returns [`slopos_utils::clock::uptime_ms`].
//! Under `#[cfg(feature = "itests")]` the same call routes through a global
//! mock clock: when the mock is inactive (value zero) it falls back to real
//! wall time so live tests are unaffected, otherwise it returns the value set
//! by the test.  This lets unit tests drive retransmit / TIME_WAIT / keepalive
//! behavior deterministically without plumbing a clock generic through every
//! TCP function.

/// Abstract source of monotonic milliseconds.
///
/// Only exists so that alternate clocks can be constructed in tests without
/// touching the global state; the production TCP state machine reads
/// [`now_ms`] directly and ignores this trait.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// The default production clock.  Returns the host's monotonic uptime in ms.
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now_ms(&self) -> u64 {
        slopos_utils::clock::uptime_ms()
    }
}

/// Read the current time in milliseconds.
///
/// Production builds resolve directly to `slopos_utils::clock::uptime_ms()`.
#[cfg(not(feature = "itests"))]
#[inline]
pub fn now_ms() -> u64 {
    slopos_utils::clock::uptime_ms()
}

/// Read the current time in milliseconds.
///
/// Test builds consult the mock clock; a value of zero means "pass through to
/// real wall time" so that live tests (which never install a mock) keep using
/// `slopos_utils::clock::uptime_ms()`.
#[cfg(feature = "itests")]
#[inline]
pub fn now_ms() -> u64 {
    mock::current_ms()
}

// -----------------------------------------------------------------------------
// Mock clock (itests only)
// -----------------------------------------------------------------------------

#[cfg(feature = "itests")]
pub use mock::{MOCK_CLOCK, MockClock};

#[cfg(feature = "itests")]
mod mock {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Global mock clock value.
    ///
    /// - `0` = inactive.  `now_ms()` falls back to `uptime_ms()`.
    /// - any non-zero value = explicit mock time in milliseconds.
    pub static MOCK_CLOCK: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub fn current_ms() -> u64 {
        let m = MOCK_CLOCK.load(Ordering::Relaxed);
        if m == 0 {
            slopos_utils::clock::uptime_ms()
        } else {
            m
        }
    }

    /// Test-facing handle for the mock clock.
    ///
    /// Has no state — the mock value lives in [`MOCK_CLOCK`].  This struct
    /// exists so tests can write `MockClock::install_at(1)` rather than poke
    /// the atomic directly.
    pub struct MockClock;

    impl MockClock {
        /// Activate the mock clock at `v` (must be non-zero; `1` is the usual
        /// starting value to distinguish "installed at t=0" from "inactive").
        pub fn install_at(v: u64) {
            MOCK_CLOCK.store(v.max(1), Ordering::Relaxed);
        }

        /// Advance the mock clock by `delta` milliseconds.
        pub fn advance(delta: u64) {
            MOCK_CLOCK.fetch_add(delta, Ordering::Relaxed);
        }

        /// Disable the mock clock; subsequent [`super::now_ms`] calls pass
        /// through to the real uptime again.
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
}
