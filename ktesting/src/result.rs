//! Outcome of running a single test entry.
//!
//! `TestOutcome` is the canonical name. `TestResult` is kept as a public
//! alias so the 70+ assertion macros that already write
//! `return $crate::TestResult::Fail` keep compiling unchanged. Phase 2's
//! site migration will rename the references at the source level; this
//! file's alias goes away then.

use core::ffi::c_int;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TestOutcome {
    Pass = 0,
    Fail = 1,
    Panic = 2,
    Skipped = 3,
    OverTime = 4,
}

/// Backward-compatible alias for `TestOutcome`. Existing assertion
/// macros and call sites use `TestResult::Fail` — the alias makes that
/// resolve to `TestOutcome::Fail` without source-level churn.
pub type TestResult = TestOutcome;

impl TestOutcome {
    #[inline]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass | Self::Skipped | Self::OverTime)
    }

    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Panic)
    }

    #[inline]
    pub fn to_c_int(self) -> c_int {
        match self {
            Self::Pass | Self::Skipped | Self::OverTime => 0,
            Self::Fail | Self::Panic => -1,
        }
    }

    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Pass,
            1 => Self::Fail,
            2 => Self::Panic,
            3 => Self::Skipped,
            4 => Self::OverTime,
            _ => Self::Fail,
        }
    }

    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// KTAP status word: "ok" for pass-equivalent, "not ok" for failures.
    #[inline]
    pub fn ktap_word(self) -> &'static str {
        if self.is_pass() {
            "ok"
        } else {
            "not ok"
        }
    }
}
