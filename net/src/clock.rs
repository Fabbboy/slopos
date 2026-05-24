//! Unified monotonic clock for the networking stack.
//!
//! Every time-dependent net subsystem — the TCP state machine *and* the
//! [`NetTimerWheel`](crate::timer::NetTimerWheel) — reads "now" in
//! milliseconds from [`now_ms`].  Production resolves it to
//! [`slopos_kernel_services::clock::uptime_ms`] (HPET-backed monotonic
//! uptime).  Under `#[cfg(feature = "test-hooks")]` the same call routes
//! through a global mock clock: when the mock is inactive (value zero) it
//! falls back to real wall time so live tests are unaffected, otherwise it
//! returns the value set by the test.
//!
//! This single source of truth is what lets a test fast-forward the *entire*
//! net stack — retransmit, TIME_WAIT, keepalive, ARP aging, reassembly GC —
//! with one [`MockClock::advance`] instead of spinning a tick counter.  There
//! is no second tick-based time domain: the timer wheel keys deadlines off
//! these same milliseconds.

/// Abstract source of monotonic milliseconds.
///
/// Only exists so that alternate clocks can be constructed in tests without
/// touching the global state; production code reads [`now_ms`] directly and
/// ignores this trait.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// The default production clock.  Returns the host's monotonic uptime in ms.
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now_ms(&self) -> u64 {
        slopos_kernel_services::clock::uptime_ms()
    }
}

/// Read the current time in milliseconds.
///
/// Production builds resolve directly to `slopos_kernel_services::clock::uptime_ms()`.
#[cfg(not(feature = "test-hooks"))]
#[inline]
pub fn now_ms() -> u64 {
    slopos_kernel_services::clock::uptime_ms()
}

/// Read the current time in milliseconds.
///
/// Test builds consult the mock clock; a value of zero means "pass through to
/// real wall time" so that live tests (which never install a mock) keep using
/// `slopos_kernel_services::clock::uptime_ms()`.
#[cfg(feature = "test-hooks")]
#[inline]
pub fn now_ms() -> u64 {
    mock::current_ms()
}

// -----------------------------------------------------------------------------
// Mock clock (test-hooks only)
// -----------------------------------------------------------------------------

#[cfg(feature = "test-hooks")]
pub use mock::{MOCK_CLOCK, MockClock, MockClockGuard};

#[cfg(feature = "test-hooks")]
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
            slopos_kernel_services::clock::uptime_ms()
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

    /// RAII guard that pins the mock clock for a test's lifetime and
    /// restores it to inactive (real wall time) on drop.
    ///
    /// `MOCK_CLOCK` is a *process-global* override of the single clock the
    /// production net stack reads (TCP RTO/retransmit, the `NetTimerWheel`,
    /// ARP aging, reassembly GC).  A test that pins it and returns without
    /// clearing leaks a frozen value into every later test and, worse, into
    /// the userland test phase — where net time would sit frozen ~1 s while
    /// real uptime climbs, so no net timer ever fires (connections then stall
    /// until a real-time deadline or the harness poll cap).  Holding this
    /// guard makes the restore automatic, *including* the early return the
    /// `assert_*!` macros take on failure, which a trailing `clear()` call
    /// would skip.
    ///
    /// Bind it to a named local for the whole test body (`let _clock = …`).
    /// Never `let _ = …` or call it as a bare statement: both drop the guard
    /// immediately and reactivate the leak.
    #[must_use = "the clock is restored when the guard drops; bind it to a named local for the test body"]
    pub struct MockClockGuard;

    impl MockClockGuard {
        /// Activate the mock clock at `v` (see [`MockClock::install_at`]) and
        /// return a guard that clears it on drop.
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
