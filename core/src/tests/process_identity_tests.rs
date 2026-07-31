//! Cross-crate tests for the two halves of a process identity.
//!
//! A process id names an address space *and* a descriptor table. mm owns
//! the first and fs the second, and neither crate can see the other's
//! half — mm sits below fs in the crate graph. These tests live in `core`
//! because it is the lowest crate that can name both, and they assert the
//! property that keeps a recycled id from inheriting its predecessor's
//! open files.

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_fs::fileio::{fileio_create_table_for_process, fileio_destroy_table_for_process};
use slopos_mm::process_vm::{create_process_vm, destroy_process_vm, init_process_vm};
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::assert_test;

/// A pid that already carries a descriptor table is refused a second one.
///
/// Answering "success" without creating anything is only harmless while
/// ids are never reused. Once they are, the second caller is a different
/// process, and it would start life holding the first one's descriptors —
/// including its stdin, its sockets and its open files.
pub fn test_fileio_create_table_rejects_a_bound_pid() -> TestResult {
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("PROC_ID_TEST: could not create a process VM");
        return TestResult::Fail;
    }

    let first = fileio_create_table_for_process(pid);
    let second = fileio_create_table_for_process(pid);

    fileio_destroy_table_for_process(pid);
    destroy_process_vm(pid);

    assert_test!(first == 0, "first fd-table create for a fresh pid failed");
    assert_test!(
        second == -1,
        "a second fd-table create for a bound pid reported success without \
         creating one — the caller would inherit the first table"
    );
    TestResult::Pass
}

/// `init_process_vm` releases descriptor tables along with address spaces.
///
/// It returns every process id to the allocator, so anything still keyed
/// on those ids has to go with them. The registered fs teardown is what
/// makes that true; without it the ids come back bound to tables their
/// new holders never opened, and the very next `task_build` is refused a
/// descriptor table.
pub fn test_init_process_vm_releases_fd_tables() -> TestResult {
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("PROC_ID_TEST: could not create a process VM");
        return TestResult::Fail;
    }
    if fileio_create_table_for_process(pid) != 0 {
        klog_info!("PROC_ID_TEST: could not create an fd table for pid {}", pid);
        fileio_destroy_table_for_process(pid);
        destroy_process_vm(pid);
        return TestResult::Fail;
    }

    init_process_vm();

    // The id is free again, so a fresh holder must be able to claim a
    // table under it. A leftover binding shows up here as a refusal.
    let rebound = fileio_create_table_for_process(pid);
    fileio_destroy_table_for_process(pid);

    assert_test!(
        rebound == 0,
        "init_process_vm left an fd table bound to a released process id"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_fileio_create_table_rejects_a_bound_pid,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_init_process_vm_releases_fd_tables,
    suite = process_identity
);
