//! Type-safe assertion macros returning TestResult on failure.

#[macro_export]
macro_rules! assert_eq_test {
    ($left:expr, $right:expr) => {{
        let left = $left;
        let right = $right;
        if left != right {
            $crate::klog_info!("ASSERT_EQ: expected {:?}, got {:?}", right, left);
            return $crate::testing::TestResult::Fail;
        }
    }};
    ($left:expr, $right:expr, $msg:expr) => {{
        let left = $left;
        let right = $right;
        if left != right {
            $crate::klog_info!("ASSERT_EQ: {} - expected {:?}, got {:?}", $msg, right, left);
            return $crate::testing::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_ne_test {
    ($left:expr, $right:expr) => {{
        let left = $left;
        let right = $right;
        if left == right {
            $crate::klog_info!("ASSERT_NE: values should differ, both are {:?}", left);
            return $crate::testing::TestResult::Fail;
        }
    }};
    ($left:expr, $right:expr, $msg:expr) => {{
        let left = $left;
        let right = $right;
        if left == right {
            $crate::klog_info!("ASSERT_NE: {} - both are {:?}", $msg, left);
            return $crate::testing::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_not_null {
    ($ptr:expr) => {{
        if $ptr.is_null() {
            $crate::klog_info!("ASSERT_NOT_NULL: pointer is null");
            return $crate::testing::TestResult::Fail;
        }
    }};
    ($ptr:expr, $msg:expr) => {{
        if $ptr.is_null() {
            $crate::klog_info!("ASSERT_NOT_NULL: {}", $msg);
            return $crate::testing::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_test {
    ($cond:expr) => {{
        if !$cond {
            $crate::klog_info!("ASSERT: condition failed");
            return $crate::testing::TestResult::Fail;
        }
    }};
    ($cond:expr, $msg:expr) => {{
        if !$cond {
            $crate::klog_info!("ASSERT: {}", $msg);
            return $crate::testing::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_zero {
    ($val:expr) => {{
        let val = $val;
        if val != 0 {
            $crate::klog_info!("ASSERT_ZERO: expected 0, got {}", val);
            return $crate::testing::TestResult::Fail;
        }
    }};
    ($val:expr, $msg:expr) => {{
        let val = $val;
        if val != 0 {
            $crate::klog_info!("ASSERT_ZERO: {} - got {}", $msg, val);
            return $crate::testing::TestResult::Fail;
        }
    }};
}

#[macro_export]
macro_rules! assert_ok {
    ($result:expr) => {{
        match $result {
            Ok(v) => v,
            Err(e) => {
                $crate::klog_info!("ASSERT_OK: got Err({:?})", e);
                return $crate::testing::TestResult::Fail;
            }
        }
    }};
    ($result:expr, $msg:expr) => {{
        match $result {
            Ok(v) => v,
            Err(e) => {
                $crate::klog_info!("ASSERT_OK: {} - got Err({:?})", $msg, e);
                return $crate::testing::TestResult::Fail;
            }
        }
    }};
}
