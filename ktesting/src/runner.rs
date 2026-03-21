use super::TestResult;

pub fn run_single_test(_name: &str, test_fn: fn() -> TestResult) -> TestResult {
    // Run the test directly. Panics propagate to the default panic handler,
    // which exits QEMU with a failure code in test mode. This avoids
    // deadlocks from longjmp through held locks or corrupted state.
    // When intentional panic-testing is needed, add a dedicated
    // run_single_test_expect_panic() that uses catch_panic!.
    test_fn()
}
