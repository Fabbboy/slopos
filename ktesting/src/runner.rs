use super::TestResult;

pub fn run_single_test(name: &str, test_fn: fn() -> TestResult) -> TestResult {
    let result = test_fn();
    if !result.is_pass() {
        slopos_utils::klog_info!("TEST FAIL: {}: {:?}", name, result);
    }
    result
}
