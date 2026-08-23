//! The kernel-I/O service threads must outlive a test scope: nothing respawns
//! one, and every path that could take one is silent about it.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskPriority, TaskStatus};
use slopos_ostd::klog_info;
use slopos_ostd::sync::kernel_io_task::{
    for_each_kernel_io_stop, kernel_io_freeze_requested, kernel_io_task_ids,
};
use slopos_testing::{TestResult, assert_test, fail};

use super::task::{
    freeze_kernel_io_all, is_infrastructure_task, task_create, task_find_by_id,
    task_registry_reset, task_shutdown_population, task_terminate,
};
use super::test_fixture::{KernelTestScope, dummy_task_entry};

fn live_kernel_io_task_ids() -> ([u32; 8], usize) {
    let mut ids = [INVALID_TASK_ID; 8];
    let mut len = 0usize;
    for_each_kernel_io_stop(|stop| {
        let id = stop.task_id();
        if id != INVALID_TASK_ID && !stop.has_exited() && len < ids.len() {
            ids[len] = id;
            len += 1;
        }
    });
    (ids, len)
}

fn all_still_registered(ids: &[u32], what: &str) -> Option<TestResult> {
    for id in ids {
        let Some(task) = task_find_by_id(*id) else {
            klog_info!("KERNEL_IO_TEST: task {} was retired by {}", id, what);
            return Some(fail!("a kernel-I/O thread was retired"));
        };
        if matches!(
            task.status(),
            TaskStatus::Terminated | TaskStatus::Zombie | TaskStatus::Invalid
        ) {
            klog_info!(
                "KERNEL_IO_TEST: task {} was killed by {} (status {:?})",
                id,
                what,
                task.status()
            );
            return Some(fail!("a kernel-I/O thread was killed"));
        }
    }
    None
}

pub fn test_kernel_io_threads_survive_a_test_scope() -> TestResult {
    let (ids, len) = live_kernel_io_task_ids();
    if len == 0 {
        return TestResult::Skipped;
    }

    {
        let _scope = KernelTestScope::enter();
    }

    if let Some(failed) = all_still_registered(&ids[..len], "a KernelTestScope round trip") {
        return failed;
    }
    TestResult::Pass
}

pub fn test_population_sweep_spares_kernel_io() -> TestResult {
    let (ids, len) = live_kernel_io_task_ids();
    if len == 0 {
        return TestResult::Skipped;
    }

    let _scope = KernelTestScope::enter();
    task_shutdown_population();

    all_still_registered(&ids[..len], "task_shutdown_population").unwrap_or(TestResult::Pass)
}

pub fn test_registry_reset_spares_kernel_io() -> TestResult {
    let (ids, len) = live_kernel_io_task_ids();
    if len == 0 {
        return TestResult::Skipped;
    }

    let _scope = KernelTestScope::enter();
    let freeze = freeze_kernel_io_all();
    let rc = task_registry_reset(&freeze);
    drop(freeze);
    if rc != 0 {
        return fail!("task_registry_reset failed");
    }

    all_still_registered(&ids[..len], "task_registry_reset").unwrap_or(TestResult::Pass)
}

pub fn test_infrastructure_is_keyed_on_the_stop_registry() -> TestResult {
    let _scope = KernelTestScope::enter();
    let kernel_io = kernel_io_task_ids();

    let ordinary = task_create(
        b"NotInfra\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if ordinary == INVALID_TASK_ID {
        return fail!("task creation failed");
    }
    let bare_kernel_io = task_create(
        b"BareKernelIo\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::KernelIo.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if bare_kernel_io == INVALID_TASK_ID {
        let _ = task_terminate(ordinary);
        return fail!("task creation failed");
    }

    let verdict = (|| {
        let ordinary_task = task_find_by_id(ordinary)?;
        let bare_task = task_find_by_id(bare_kernel_io)?;
        Some((
            is_infrastructure_task(&ordinary_task, &kernel_io),
            is_infrastructure_task(&bare_task, &kernel_io),
        ))
    })();

    let _ = task_terminate(ordinary);
    let _ = task_terminate(bare_kernel_io);

    let Some((ordinary_is_infra, bare_is_infra)) = verdict else {
        return fail!("a just-created task did not resolve");
    };
    assert_test!(!ordinary_is_infra, "an ordinary task is not infrastructure");
    assert_test!(
        !bare_is_infra,
        "a KernelIo task with no registered stop is not infrastructure"
    );

    let (ids, len) = live_kernel_io_task_ids();
    for id in &ids[..len] {
        let Some(task) = task_find_by_id(*id) else {
            return fail!("a registered kernel-I/O thread did not resolve");
        };
        assert_test!(
            is_infrastructure_task(&task, &kernel_io),
            "a registered kernel-I/O thread is infrastructure"
        );
    }
    TestResult::Pass
}

pub fn test_kernel_io_freeze_nests() -> TestResult {
    assert_test!(
        !kernel_io_freeze_requested(),
        "a freeze is held before the test started"
    );
    let outer = freeze_kernel_io_all();
    assert_test!(
        kernel_io_freeze_requested(),
        "the outer freeze did not take"
    );
    {
        let _inner = freeze_kernel_io_all();
        assert_test!(
            kernel_io_freeze_requested(),
            "the inner freeze did not take"
        );
    }
    assert_test!(
        kernel_io_freeze_requested(),
        "releasing the inner freeze thawed the outer holder's threads"
    );
    drop(outer);
    assert_test!(
        !kernel_io_freeze_requested(),
        "releasing the last freeze did not thaw"
    );
    TestResult::Pass
}

pub fn test_kernel_io_threads_run_after_a_test_scope() -> TestResult {
    let mut before = [0u64; 8];
    let mut len = 0usize;
    for_each_kernel_io_stop(|stop| {
        if stop.task_id() != INVALID_TASK_ID && !stop.has_exited() && len < before.len() {
            before[len] = stop.laps();
            len += 1;
        }
    });
    if len == 0 {
        return TestResult::Skipped;
    }

    {
        let _scope = KernelTestScope::enter();
    }

    const BUDGET_MS: u64 = 2_000;
    let deadline = slopos_kernel_services::platform::get_time_ms().saturating_add(BUDGET_MS);
    loop {
        let mut advanced = false;
        let mut idx = 0usize;
        for_each_kernel_io_stop(|stop| {
            if stop.task_id() != INVALID_TASK_ID && !stop.has_exited() && idx < before.len() {
                if stop.laps() > before[idx] {
                    advanced = true;
                }
                idx += 1;
            }
        });
        if advanced {
            return TestResult::Pass;
        }
        if slopos_kernel_services::platform::get_time_ms() >= deadline {
            klog_info!("KERNEL_IO_TEST: no kernel-I/O thread advanced a lap in {BUDGET_MS} ms");
            return fail!("kernel-I/O threads stopped being dispatched");
        }
        crate::scheduler::yield_();
    }
}

slopos_testing::stest!(
    name = test_kernel_io_threads_survive_a_test_scope,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_population_sweep_spares_kernel_io,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_registry_reset_spares_kernel_io,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_infrastructure_is_keyed_on_the_stop_registry,
    suite = kernel_io
);
slopos_testing::stest!(name = test_kernel_io_freeze_nests, suite = kernel_io);
slopos_testing::stest!(
    name = test_kernel_io_threads_run_after_a_test_scope,
    suite = kernel_io
);
