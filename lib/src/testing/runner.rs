use super::TestResult;

pub fn run_single_test(_name: &str, test_fn: fn() -> TestResult) -> TestResult {
    let result = crate::catch_panic!({ test_fn().to_c_int() });

    if result == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}
