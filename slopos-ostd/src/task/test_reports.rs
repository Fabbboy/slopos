//! Per-task ring buffer for `SYSCALL_TEST_REPORT` payloads.
//!
//! A user task's first `SYSCALL_TEST_REPORT` lazily allocates one of these
//! rings into `Task::test_reports`. Non-test tasks never call the syscall
//! and pay zero cost. After the task exits, the kernel-side userland-test
//! runner calls `task_drain_test_reports` (in the kernel-side scheduler)
//! to take ownership of the ring and read out the recorded subtests.

use slopos_abi::syscall::{TEST_REPORT_MSG_MAX, TEST_REPORT_NAME_MAX, TEST_REPORT_RING_CAPACITY};

use crate::{AllocError, KBox, KVec, Zeroable};

/// One subtest result. `name`/`msg` are length-prefixed byte arrays — the
/// `*_len` fields hold the populated prefix length; the remainder is zero.
#[derive(Clone, Copy, Zeroable)]
#[repr(C)]
pub struct TestReport {
    pub status: u8,
    pub name_len: u8,
    pub msg_len: u8,
    _pad: u8,
    pub name: [u8; TEST_REPORT_NAME_MAX],
    pub msg: [u8; TEST_REPORT_MSG_MAX],
}

/// Bounded per-task ring of `TestReport`. Newest report is dropped on
/// overflow and the `overflow` flag is latched so the runner can flag
/// truncation in its KTAP output.
#[derive(Zeroable)]
#[repr(C)]
pub struct TestReportRing {
    count: u16,
    overflow: u8,
    _pad: u8,
    entries: [TestReport; TEST_REPORT_RING_CAPACITY],
}

impl TestReportRing {
    pub fn push(&mut self, r: TestReport) {
        let cap = self.entries.len();
        let idx = self.count as usize;
        if idx >= cap {
            self.overflow = 1;
            return;
        }
        self.entries[idx] = r;
        self.count += 1;
    }

    /// Move every recorded `TestReport` out of the ring into a heap vector,
    /// resetting the ring to empty in place.
    pub fn drain(&mut self) -> Result<KVec<TestReport>, AllocError> {
        let mut out: KVec<TestReport> = KVec::new();
        for i in 0..self.count as usize {
            out.push(self.entries[i])?;
        }
        self.count = 0;
        self.overflow = 0;
        Ok(out)
    }

    pub fn overflow_flag(&self) -> bool {
        self.overflow != 0
    }
}

/// Heap-allocate a fresh zeroed ring. Uses in-place init so the ~12 KiB
/// `TestReportRing` rvalue never lands on the caller's stack — required to
/// stay under the 2 KiB stack-frame gate.
pub fn alloc_ring() -> Result<KBox<TestReportRing>, AllocError> {
    KBox::<TestReportRing>::zeroed()
}

/// Construct an empty `TestReport` so users don't need to spell out the
/// padding byte. Used by the syscall handler when copying name/msg buffers.
pub fn empty_report() -> TestReport {
    TestReport {
        status: 0,
        name_len: 0,
        msg_len: 0,
        _pad: 0,
        name: [0; TEST_REPORT_NAME_MAX],
        msg: [0; TEST_REPORT_MSG_MAX],
    }
}

// =============================================================================
