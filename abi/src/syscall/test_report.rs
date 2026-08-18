//! ABI types and constants for the userland test-harness syscall boundary,
//! keeping `slibc::test_harness` and `core::syscall::test_handlers` in sync.

/// Maximum bytes copied in for the test name. Longer names are truncated.
pub const TEST_REPORT_NAME_MAX: usize = 64;

/// Maximum bytes copied in for the optional diagnostic message.
pub const TEST_REPORT_MSG_MAX: usize = 128;

/// Per-task report ring capacity. Reports beyond this count are dropped and
/// the ring's overflow flag is set so KTAP output can mark the truncation.
pub const TEST_REPORT_RING_CAPACITY: usize = 64;

/// Status carried in `arg0` of `SYSCALL_TEST_REPORT`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestReportStatus {
    Pass = 0,
    Fail = 1,
    Skip = 2,
}
