//! Cross-crate tests for the two halves of a process identity.
//!
//! A process id names an address space *and* a descriptor table. mm owns the
//! first and fs the second, so these live in `core` — the lowest crate that
//! can name both.

use core::ffi::c_int;

use slopos_abi::fs::O_RDONLY;
use slopos_fs::fileio::{
    file_open_for_process, file_pipe_create, fileio_create_table_for_process,
    fileio_destroy_table_for_process, fileio_get_open_file_handle,
};
use slopos_mm::process_vm::{destroy_process_vm, init_process_vm};
use slopos_ostd::klog_info;
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;
use slopos_testing::assert_test;

/// A pid that already carries a descriptor table is refused a second one: once
/// ids recycle, the second caller is a different process and would start life
/// holding the first one's stdin, sockets and open files.
pub fn test_fileio_create_table_rejects_a_bound_process() -> TestResult {
    let Ok(process) = slopos_ostd::process::process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a process");
        return TestResult::Fail;
    };
    let Some(handle) = process.handle() else {
        klog_info!("PROC_ID_TEST: a registered process carries no handle");
        return TestResult::Fail;
    };

    let first = fileio_create_table_for_process(handle);
    let second = fileio_create_table_for_process(handle);

    fileio_destroy_table_for_process(handle);
    slopos_ostd::process::process_retire(handle);

    assert_test!(
        first == 0,
        "first fd-table create for a fresh process failed"
    );
    assert_test!(
        second == -1,
        "a second fd-table create for a bound pid reported success without \
         creating one — the caller would inherit the first table"
    );
    TestResult::Pass
}

/// A fixture reset releases descriptor tables along with address spaces.
///
/// A leftover binding means the ids come back bound to tables their new holders
/// never opened, and the very next `task_build` is refused a descriptor table.
pub fn test_a_fixture_reset_releases_fd_tables() -> TestResult {
    // The reset destroys every descriptor table and every address space in the
    // machine, so it may not run against live APs or a running kernel-I/O
    // thread; the scope parks both for the duration.
    let _scope = KernelTestScope::enter();

    let Ok(process) = slopos_ostd::process::process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a process");
        return TestResult::Fail;
    };
    let Some(handle) = process.handle() else {
        return TestResult::Fail;
    };
    if fileio_create_table_for_process(handle) != 0 {
        klog_info!("PROC_ID_TEST: could not create an fd table");
        fileio_destroy_table_for_process(handle);
        return TestResult::Fail;
    }

    slopos_fs::fileio_reset_all_tables();
    init_process_vm();

    let Ok(fresh) = slopos_ostd::process::process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a process after the reset");
        return TestResult::Fail;
    };
    let Some(fresh_handle) = fresh.handle() else {
        return TestResult::Fail;
    };
    let rebound = fileio_create_table_for_process(fresh_handle);
    fileio_destroy_table_for_process(fresh_handle);
    slopos_ostd::process::process_retire(fresh_handle);

    assert_test!(
        rebound == 0,
        "the reset left an fd table bound to a released process slot"
    );
    TestResult::Pass
}

/// A recycled process id does not resolve to the prior principal.
///
/// Asserted end to end across all three tables. The lowest-free allocator
/// guarantees the id is reissued immediately, so the second process is a
/// *different* principal every designator minted against the first must say so
/// about. A handle resolving to the second is the confused deputy: the kernel
/// servicing a fault or an open in a stranger's tables.
pub fn test_a_recycled_pid_does_not_resolve_to_the_prior_principal() -> TestResult {
    use slopos_ostd::process::{process_for_handle, process_retire, process_spawn_root};

    // Lowest-free allocation only reissues the id if nothing else takes it in
    // between, and every AP is free to spawn.
    let _scope = KernelTestScope::enter();

    let Ok(first) = process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a process");
        return TestResult::Fail;
    };
    let first_id = first.id();
    let Some(stale) = first.handle() else {
        klog_info!("PROC_ID_TEST: a registered process carries no handle");
        return TestResult::Fail;
    };
    if slopos_mm::process_vm::create_process_vm_for(first.clone()).is_none() {
        klog_info!("PROC_ID_TEST: could not give the first process an address space");
        return TestResult::Fail;
    }
    if slopos_fs::fileio::fileio_create_table_for_process(stale) != 0 {
        klog_info!("PROC_ID_TEST: could not give the first process a descriptor table");
        destroy_process_vm(slopos_ostd::process::ProcessId::resolve(first_id).expect("live"));
        return TestResult::Fail;
    }

    // The last reference is what returns the id to the allocator.
    slopos_fs::fileio::fileio_destroy_table_for_process(stale);
    destroy_process_vm(slopos_ostd::process::ProcessId::resolve(first_id).expect("live"));
    process_retire(stale);
    drop(first);

    let Ok(second) = process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a second process");
        return TestResult::Fail;
    };
    let second_id = second.id();
    let second_handle = second.handle();
    let resolved_stale = process_for_handle(stale).map(|p| p.id());
    let vm_after = slopos_mm::process_vm::process_vm_handle_for(stale).is_some();
    let table_after = slopos_fs::fileio::fileio_table_exists_for_process(stale);

    if let Some(handle) = second_handle {
        process_retire(handle);
    }
    drop(second);

    assert_test!(
        second_id == first_id,
        "the id was not reissued ({second_id} != {first_id}) — this test proves \
         nothing unless the number actually recycles"
    );
    assert_test!(
        resolved_stale.is_none(),
        "a handle to the retired process resolved to process {resolved_stale:?}, \
         which now carries the same id — the confused deputy"
    );
    assert_test!(
        !vm_after,
        "a stale handle still names an address space after its process was reaped"
    );
    assert_test!(
        !table_after,
        "a stale handle still names a descriptor table after its process was reaped"
    );
    TestResult::Pass
}

/// Descriptors are never installed into the kernel's table on behalf of a
/// process that has none of its own.
///
/// The kernel table is shared by every kernel task, so "this pid owns no
/// table" has to answer with a refusal, never a redirect into a more
/// privileged domain.
pub fn test_fileio_refuses_a_process_with_no_table() -> TestResult {
    // `FdTable::Kernel` is the one table every kernel task shares, so counting
    // it is only a measurement of this call while no other task can open or
    // close a descriptor.
    let _scope = KernelTestScope::enter();

    let Ok(process) = slopos_ostd::process::process_spawn_root() else {
        klog_info!("PROC_ID_TEST: could not register a process");
        return TestResult::Fail;
    };
    let Some(table) = slopos_fs::fileio::FdTable::of(&process) else {
        return TestResult::Fail;
    };
    // Deliberately no `fileio_create_table_for_process`.

    let before = kernel_table_open_fds();

    let mut read_fd: c_int = -1;
    let mut write_fd: c_int = -1;
    let pipe_rc = file_pipe_create(table, 0, &mut read_fd, &mut write_fd);
    let open_rc = file_open_for_process(table, b"/", O_RDONLY);

    let after = kernel_table_open_fds();
    if let Some(handle) = process.handle() {
        slopos_ostd::process::process_retire(handle);
    }

    assert_test!(
        pipe_rc == ESRCH_RC,
        "pipe create for a process with no table returned {pipe_rc}, want ESRCH"
    );
    assert_test!(
        open_rc < 0,
        "open for a process with no table returned fd {open_rc} instead of failing"
    );
    assert_test!(
        before == after,
        "kernel fd table grew from {before} to {after} descriptors while \
         serving a process that owns no table"
    );
    TestResult::Pass
}

fn kernel_table_open_fds() -> usize {
    (0..MAX_PROBE_FD)
        .filter(|fd| fileio_get_open_file_handle(slopos_fs::fileio::FdTable::Kernel, *fd).is_some())
        .count()
}

/// Past any per-process descriptor bound; out-of-range probes read as unused.
const MAX_PROBE_FD: c_int = 1024;

const ESRCH_RC: c_int = -3;

slopos_testing::stest!(
    name = test_fileio_create_table_rejects_a_bound_process,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_fileio_refuses_a_process_with_no_table,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_a_fixture_reset_releases_fd_tables,
    suite = process_identity
);
slopos_testing::stest!(
    name = test_a_recycled_pid_does_not_resolve_to_the_prior_principal,
    suite = process_identity
);
