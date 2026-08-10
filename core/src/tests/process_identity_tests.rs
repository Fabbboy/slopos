//! Cross-crate tests for the two halves of a process identity.
//!
//! A process id names an address space *and* a descriptor table. mm owns
//! the first and fs the second, and neither crate can see the other's
//! half — mm sits below fs in the crate graph. These tests live in `core`
//! because it is the lowest crate that can name both, and they assert the
//! property that keeps a recycled id from inheriting its predecessor's
//! open files.

use core::ffi::c_int;

use slopos_abi::fs::O_RDONLY;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_fs::fileio::{
    file_open_for_process, file_pipe_create, fileio_create_table_for_process,
    fileio_destroy_table_for_process, fileio_get_open_file_handle,
};
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

/// A fixture reset releases descriptor tables along with address spaces.
///
/// A process id names both, so returning one has to return the other. That
/// used to depend on a function pointer mm had installed at boot, because mm
/// sits below fs and could not name the teardown; it now depends on the two
/// resets being called together, which is a call the test can make directly
/// and a reader can see.
///
/// The failure this catches is unchanged: a leftover binding means the ids
/// come back bound to tables their new holders never opened, and the very
/// next `task_build` is refused a descriptor table.
pub fn test_a_fixture_reset_releases_fd_tables() -> TestResult {
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

    slopos_fs::fileio_reset_all_tables();
    init_process_vm();

    // The slot is free again, so a fresh holder must be able to claim a table
    // in it. A leftover binding shows up here as a refusal.
    let fresh = create_process_vm();
    if fresh == INVALID_PROCESS_ID {
        klog_info!("PROC_ID_TEST: could not create a process VM after the reset");
        return TestResult::Fail;
    }
    let rebound = fileio_create_table_for_process(fresh);
    fileio_destroy_table_for_process(fresh);
    destroy_process_vm(fresh);

    assert_test!(
        rebound == 0,
        "the reset left an fd table bound to a released process slot"
    );
    TestResult::Pass
}

/// Descriptors are never installed into the kernel's table on behalf of a
/// process that has none of its own.
///
/// The kernel table is shared by every kernel task. A user process whose
/// descriptors landed there would hand them to all of them, and would see
/// theirs — so the answer to "this pid owns no table" has to be a refusal,
/// never a redirect into a more privileged domain.
pub fn test_fileio_refuses_a_pid_with_no_table() -> TestResult {
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("PROC_ID_TEST: could not create a process VM");
        return TestResult::Fail;
    }
    // Deliberately no `fileio_create_table_for_process`.

    let before = kernel_table_open_fds();

    let mut read_fd: c_int = -1;
    let mut write_fd: c_int = -1;
    let pipe_rc = file_pipe_create(pid, 0, &mut read_fd, &mut write_fd);
    let open_rc = file_open_for_process(pid, b"/", O_RDONLY);

    let after = kernel_table_open_fds();
    destroy_process_vm(pid);

    assert_test!(
        pipe_rc == ESRCH_RC,
        "pipe create for an unregistered pid returned {pipe_rc}, want ESRCH"
    );
    assert_test!(
        open_rc < 0,
        "open for an unregistered pid returned fd {open_rc} instead of failing"
    );
    assert_test!(
        before == after,
        "kernel fd table grew from {before} to {after} descriptors while \
         serving a process that owns no table"
    );
    TestResult::Pass
}

/// Occupied descriptor count in the kernel's own table.
fn kernel_table_open_fds() -> usize {
    (0..MAX_PROBE_FD)
        .filter(|fd| fileio_get_open_file_handle(INVALID_PROCESS_ID, *fd).is_some())
        .count()
}

/// Past any per-process descriptor bound; out-of-range probes read as unused.
const MAX_PROBE_FD: c_int = 1024;

const ESRCH_RC: c_int = -3;

slopos_testing::stest!(
    name = test_fileio_create_table_rejects_a_bound_pid,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_fileio_refuses_a_pid_with_no_table,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_a_fixture_reset_releases_fd_tables,
    suite = process_identity
);
