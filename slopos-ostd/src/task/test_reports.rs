//! Per-task ring buffer for `SYSCALL_TEST_REPORT` payloads.
//!
//! A task's first `SYSCALL_TEST_REPORT` lazily allocates one into
//! `Task::test_reports`; after the task exits the userland-test runner calls
//! `task_drain_test_reports` to take ownership and read out the subtests.

use slopos_abi::syscall::{TEST_REPORT_MSG_MAX, TEST_REPORT_NAME_MAX, TEST_REPORT_RING_CAPACITY};

use crate::{AllocError, KBox, KVec, Zeroable};

/// One subtest result; `*_len` holds the populated prefix of `name`/`msg`
/// and the remainder is zero.
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

/// A report arriving past capacity is dropped and the `overflow` flag latched
/// so the runner can mark the KTAP output truncated.
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

/// In-place init keeps the ~12 KiB `TestReportRing` rvalue off the caller's
/// stack, under the 2 KiB stack-frame gate.
pub fn alloc_ring() -> Result<KBox<TestReportRing>, AllocError> {
    KBox::<TestReportRing>::zeroed()
}

/// Spares callers spelling out the private padding byte.
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
