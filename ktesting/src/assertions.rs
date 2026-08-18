//! Type-safe assertion macros returning TestResult on failure.
//!
//! Failure arms live in `#[cold] #[inline(never)]` helpers because at
//! opt-level 0 each `klog_info!` costs the *caller's* frame an unmerged
//! `Arguments` array whether or not it fires. `&dyn Debug` rather than generics
//! keeps that to one helper each instead of one per formatted type.

use core::fmt::Debug;

use slopos_ostd::klog_info;

use crate::TestResult;

#[cold]
#[inline(never)]
pub fn assert_eq_failed(msg: Option<&str>, expected: &dyn Debug, got: &dyn Debug) -> TestResult {
    match msg {
        Some(m) => klog_info!("ASSERT_EQ: {} - expected {:?}, got {:?}", m, expected, got),
        None => klog_info!("ASSERT_EQ: expected {:?}, got {:?}", expected, got),
    }
    TestResult::Fail
}

#[cold]
#[inline(never)]
pub fn assert_ne_failed(msg: Option<&str>, both: &dyn Debug) -> TestResult {
    match msg {
        Some(m) => klog_info!("ASSERT_NE: {} - both are {:?}", m, both),
        None => klog_info!("ASSERT_NE: values should differ, both are {:?}", both),
    }
    TestResult::Fail
}

#[cold]
#[inline(never)]
pub fn assert_failed(prefix: &str, msg: Option<&str>) -> TestResult {
    match msg {
        Some(m) => klog_info!("{}: {}", prefix, m),
        None => klog_info!("{}", prefix),
    }
    TestResult::Fail
}

#[cold]
#[inline(never)]
pub fn assert_zero_failed(msg: Option<&str>, got: &dyn Debug) -> TestResult {
    match msg {
        Some(m) => klog_info!("ASSERT_ZERO: {} - got {:?}", m, got),
        None => klog_info!("ASSERT_ZERO: expected 0, got {:?}", got),
    }
    TestResult::Fail
}

#[cold]
#[inline(never)]
pub fn assert_ok_failed(msg: Option<&str>, err: &dyn Debug) -> TestResult {
    match msg {
        Some(m) => klog_info!("ASSERT_OK: {} - got Err({:?})", m, err),
        None => klog_info!("ASSERT_OK: got Err({:?})", err),
    }
    TestResult::Fail
}

#[macro_export]
macro_rules! assert_eq_test {
    ($left:expr, $right:expr) => {{
        let left = $left;
        let right = $right;
        if left != right {
            return $crate::assertions::assert_eq_failed(None, &right, &left);
        }
    }};
    ($left:expr, $right:expr, $msg:expr) => {{
        let left = $left;
        let right = $right;
        if left != right {
            return $crate::assertions::assert_eq_failed(Some($msg), &right, &left);
        }
    }};
}

#[macro_export]
macro_rules! assert_ne_test {
    ($left:expr, $right:expr) => {{
        let left = $left;
        let right = $right;
        if left == right {
            return $crate::assertions::assert_ne_failed(None, &left);
        }
    }};
    ($left:expr, $right:expr, $msg:expr) => {{
        let left = $left;
        let right = $right;
        if left == right {
            return $crate::assertions::assert_ne_failed(Some($msg), &left);
        }
    }};
}

#[macro_export]
macro_rules! assert_not_null {
    ($ptr:expr) => {{
        if $ptr.is_null() {
            return $crate::assertions::assert_failed("ASSERT_NOT_NULL: pointer is null", None);
        }
    }};
    ($ptr:expr, $msg:expr) => {{
        if $ptr.is_null() {
            return $crate::assertions::assert_failed("ASSERT_NOT_NULL", Some($msg));
        }
    }};
}

/// Unwrap an `Option`, failing the test when it is `None`. Registry lookups
/// return an owning guard, so the binding must outlive every use of what it
/// names.
#[macro_export]
macro_rules! assert_some {
    ($opt:expr) => {{
        match $opt {
            Some(v) => v,
            None => {
                return $crate::assertions::assert_failed("ASSERT_SOME: value is None", None);
            }
        }
    }};
    ($opt:expr, $msg:expr) => {{
        match $opt {
            Some(v) => v,
            None => {
                return $crate::assertions::assert_failed("ASSERT_SOME", Some($msg));
            }
        }
    }};
}

#[macro_export]
macro_rules! assert_test {
    ($cond:expr) => {{
        if !$cond {
            return $crate::assertions::assert_failed("ASSERT: condition failed", None);
        }
    }};
    ($cond:expr, $msg:expr) => {{
        if !$cond {
            return $crate::assertions::assert_failed("ASSERT", Some($msg));
        }
    }};
    // This arm keeps `format_args!` at the call site: a non-generic helper
    // cannot name the types of the values being formatted.
    ($cond:expr, $fmt:expr, $($arg:tt)*) => {{
        if !$cond {
            slopos_ostd::klog_info!(concat!("ASSERT: ", $fmt), $($arg)*);
            return $crate::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_zero {
    ($val:expr) => {{
        let val = $val;
        if val != 0 {
            return $crate::assertions::assert_zero_failed(None, &val);
        }
    }};
    ($val:expr, $msg:expr) => {{
        let val = $val;
        if val != 0 {
            return $crate::assertions::assert_zero_failed(Some($msg), &val);
        }
    }};
}

#[macro_export]
macro_rules! assert_ok {
    ($result:expr) => {{
        match $result {
            Ok(v) => v,
            Err(e) => {
                return $crate::assertions::assert_ok_failed(None, &e);
            }
        }
    }};
    ($result:expr, $msg:expr) => {{
        match $result {
            Ok(v) => v,
            Err(e) => {
                return $crate::assertions::assert_ok_failed(Some($msg), &e);
            }
        }
    }};
}
