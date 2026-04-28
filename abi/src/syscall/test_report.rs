//! ABI types and constants shared across the userland test-harness syscall
//! boundary. Both `slibc::test_harness` (userland) and the kernel-side
//! `core::syscall::test_handlers` reference these so the wire-format limits
//! stay in sync.

/// Maximum bytes copied in for the test name. Longer names are truncated.
pub const TEST_REPORT_NAME_MAX: usize = 64;

/// Maximum bytes copied in for the optional diagnostic message.
pub const TEST_REPORT_MSG_MAX: usize = 128;

/// Per-task report ring capacity. Reports beyond this count are dropped and
/// the ring's overflow flag is set so the kernel runner can flag truncation
/// in its KTAP output.
pub const TEST_REPORT_RING_CAPACITY: usize = 64;

/// Status carried in `arg0` of `SYSCALL_TEST_REPORT`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestReportStatus {
    Pass = 0,
    Fail = 1,
    Skip = 2,
}
