use super::*;
use crate::tty::driver::SerialConsoleDriver;

// ===========================================================================
// TtyDriverKind tests
// ===========================================================================

/// TtyDriverKind::SerialConsole(SerialConsoleDriver) does not panic on write/drain.
pub fn test_driver_none_no_panic() -> TestResult {
    let driver = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    driver.write_output(b"test");
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - None driver returned {} from drain", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VConsoleDriver drain_input returns 0 (input is interrupt-driven).
pub fn test_vconsole_drain_returns_zero() -> TestResult {
    let driver = TtyDriverKind::VConsole(VConsoleDriver);
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - VConsole drain returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}
