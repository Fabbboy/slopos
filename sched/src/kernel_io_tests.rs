//! The kernel-I/O service threads must outlive a test scope: nothing respawns
//! one, and every path that could take one is silent about it.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskPriority, TaskStatus};
use slopos_ostd::klog_info;
use slopos_ostd::sync::kernel_io_task::{
    arm_kernel_io_hold_over_for_test, disarm_kernel_io_hold_for_test, for_each_kernel_io_stop,
    kernel_io_freeze_requested, kernel_io_held_ids, kernel_io_task_ids,
};
use slopos_ostd::task::{SchedPlacement, task_placement_strong_count};
use slopos_testing::{TestResult, assert_test, fail};

use super::per_cpu::{
    get_total_ready_tasks, hold_kernel_io_off_all_runqueues, pause_all_aps,
    resume_all_aps_if_not_nested, with_cpu_scheduler,
};
use super::scheduler::{clear_nascent_for_test, schedule_task};
use super::task::{
    Task, TaskRef, freeze_kernel_io_all, is_infrastructure_task, kernel_io_dispatchable_count,
    republish_held_kernel_io, task_create, task_find_by_id, task_registry_reset, task_set_state,
    task_shutdown_population, task_terminate,
};
use super::test_fixture::{KernelTestScope, dummy_task_entry};

/// A synthetic, never-dispatched task the hold can be armed over, so the
/// mechanism tests do not depend on a live kthread's timing.
fn make_ready_task(name: &[u8]) -> Option<(u32, TaskRef)> {
    let id = task_create(
        name.as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if id == INVALID_TASK_ID {
        return None;
    }
    if !clear_nascent_for_test(id) || task_set_state(id, TaskStatus::Ready) != 0 {
        let _ = task_terminate(id);
        return None;
    }
    task_find_by_id(id).map(|task| (id, task))
}

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

pub fn test_a_scope_takes_every_kernel_io_thread_off_every_runqueue() -> TestResult {
    let (_ids, len) = live_kernel_io_task_ids();
    if len == 0 {
        return TestResult::Skipped;
    }

    let _scope = KernelTestScope::enter();
    assert_test!(
        kernel_io_dispatchable_count() == 0,
        "a kernel-I/O thread was still owned by a scheduler container"
    );
    assert_test!(
        get_total_ready_tasks() == 0,
        "a scope started with a non-empty run queue"
    );
    TestResult::Pass
}

/// The regression test for the flake family this hold exists for: the freeze is
/// cooperative and can report anything, and quiescence must not depend on it.
pub fn test_scope_quiescence_does_not_depend_on_the_freeze_outcome() -> TestResult {
    let (_ids, len) = live_kernel_io_task_ids();
    if len == 0 {
        return TestResult::Skipped;
    }

    let scope = KernelTestScope::enter();
    klog_info!(
        "KERNEL_IO_TEST: freeze reported {:?}",
        scope.kernel_io_freeze_outcome()
    );
    assert_test!(scope.kernel_io_is_quiesced(), "the scope was not quiesced");
    TestResult::Pass
}

pub fn test_a_held_task_is_not_published_to_a_runqueue() -> TestResult {
    let _scope = KernelTestScope::enter();
    let Some((id, task)) = make_ready_task(b"HeldPublish\0") else {
        return fail!("task creation failed");
    };

    let displaced = arm_kernel_io_hold_over_for_test(&[id]);
    let ready_before = get_total_ready_tasks();
    let rc = schedule_task(&task);
    let ready_after = get_total_ready_tasks();
    let placement = task.sched_placement();
    let linked = task.ready_link.is_linked() || task.inbox_link().is_linked();
    disarm_kernel_io_hold_for_test(&displaced);

    let body: &Task = &task;
    let _ = body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::None);
    let _ = task_terminate(id);

    assert_test!(rc == 0, "publishing a held task reported failure");
    assert_test!(
        placement == SchedPlacement::Held,
        "the publish gate did not claim the task"
    );
    assert_test!(!linked, "a held task reached a scheduler container");
    assert_test!(
        ready_after == ready_before,
        "a held task reached a run queue"
    );
    TestResult::Pass
}

pub fn test_releasing_the_hold_republishes_a_held_task() -> TestResult {
    let _scope = KernelTestScope::enter();
    let Some((id, task)) = make_ready_task(b"HeldRelease\0") else {
        return fail!("task creation failed");
    };

    let displaced = arm_kernel_io_hold_over_for_test(&[id]);
    let _ = schedule_task(&task);
    let held = kernel_io_held_ids();
    let claimed = task.sched_placement() == SchedPlacement::Held;
    disarm_kernel_io_hold_for_test(&displaced);
    republish_held_kernel_io(&held);

    let placement = task.sched_placement();
    let linked = task.ready_link.is_linked() || task.inbox_link().is_linked();
    let ready = task.is_ready();
    let _ = task_terminate(id);

    assert_test!(claimed, "the publish gate did not claim the task");
    assert_test!(ready, "the released task was not left Ready");
    assert_test!(linked, "the released task was not queued again");
    assert_test!(
        matches!(
            placement,
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake
        ),
        "the released task did not reach a scheduler container"
    );
    TestResult::Pass
}

pub fn test_the_sweep_takes_a_queued_task_off_the_queue() -> TestResult {
    let _scope = KernelTestScope::enter();
    let Some((id, task)) = make_ready_task(b"SweptQueued\0") else {
        return fail!("task creation failed");
    };
    // Enqueued locally rather than published, so the node is on a queue this
    // CPU can then watch the sweep take it off.
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let queued = with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(&task)) == Some(0);
    if !queued || !task.ready_link.is_linked() {
        let _ = task_terminate(id);
        return fail!("the fixture task did not reach a run queue");
    }

    let displaced = arm_kernel_io_hold_over_for_test(&[id]);
    // Nested, so the sweep inside a scope still has the token its precondition
    // is stated in.
    let Ok(paused) = pause_all_aps() else {
        disarm_kernel_io_hold_for_test(&displaced);
        let _ = task_terminate(id);
        return fail!("nested AP pause failed");
    };
    let swept = hold_kernel_io_off_all_runqueues(&paused);
    resume_all_aps_if_not_nested(paused);

    let placement = task.sched_placement();
    let linked = task.ready_link.is_linked();
    let held = kernel_io_held_ids();
    disarm_kernel_io_hold_for_test(&displaced);
    republish_held_kernel_io(&held);
    let _ = task_terminate(id);

    assert_test!(swept == 1, "the sweep did not take exactly one task");
    assert_test!(!linked, "the swept task was still linked");
    assert_test!(
        placement == SchedPlacement::Held,
        "the swept task was not left Held"
    );
    TestResult::Pass
}

pub fn test_the_inbox_drain_hands_a_held_task_to_the_hold() -> TestResult {
    let _scope = KernelTestScope::enter();
    let Some((id, task)) = make_ready_task(b"HeldInbox\0") else {
        return fail!("task creation failed");
    };
    let node = task.node();
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let strong_base = task_placement_strong_count(node);

    let displaced = arm_kernel_io_hold_over_for_test(&[id]);
    let _ = with_cpu_scheduler(cpu_id, |sched| sched.push_remote_wake(&task));
    let pushed = task.sched_placement() == SchedPlacement::RemoteWake;
    let _ = with_cpu_scheduler(cpu_id, |sched| sched.drain_remote_inbox());

    let placement = task.sched_placement();
    let linked = task.ready_link.is_linked();
    let inbox_linked = task.inbox_link().is_linked();
    let strong_after = task_placement_strong_count(node);
    disarm_kernel_io_hold_for_test(&displaced);

    let body: &Task = &task;
    let _ = body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::None);
    let _ = task_terminate(id);

    assert_test!(pushed, "the inbox push did not take");
    assert_test!(
        placement == SchedPlacement::Held,
        "the drain did not hand the task to the hold"
    );
    assert_test!(!linked, "a held task reached a ready queue");
    assert_test!(!inbox_linked, "the inbox link survived the drain");
    assert_test!(
        strong_after == strong_base,
        "the inbox reference was not released exactly once"
    );
    TestResult::Pass
}

/// The I2/I7 regression test: the hold parks no reference of its own, so every
/// step of sweep-and-release must move the strong count by exactly the queue
/// membership it takes or gives back.
pub fn test_holding_and_releasing_a_held_task_is_refcount_neutral() -> TestResult {
    let _scope = KernelTestScope::enter();
    let Some((id, task)) = make_ready_task(b"HeldRefcount\0") else {
        return fail!("task creation failed");
    };
    let node = task.node();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let base = task_placement_strong_count(node);
    let queued = with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(&task)) == Some(0);
    let after_queue = task_placement_strong_count(node);

    let displaced = arm_kernel_io_hold_over_for_test(&[id]);
    let Ok(paused) = pause_all_aps() else {
        disarm_kernel_io_hold_for_test(&displaced);
        let _ = task_terminate(id);
        return fail!("nested AP pause failed");
    };
    let _ = hold_kernel_io_off_all_runqueues(&paused);
    resume_all_aps_if_not_nested(paused);
    let after_sweep = task_placement_strong_count(node);

    let held = kernel_io_held_ids();
    disarm_kernel_io_hold_for_test(&displaced);
    republish_held_kernel_io(&held);
    let after_release = task_placement_strong_count(node);

    let _ = task_terminate(id);

    assert_test!(queued, "the fixture task did not reach a run queue");
    assert_test!(
        after_queue == base + 1,
        "queue membership did not park exactly one reference"
    );
    assert_test!(
        after_sweep == base,
        "the sweep did not release the queue's membership reference exactly once"
    );
    assert_test!(
        after_release == base + 1,
        "the release did not park exactly one fresh membership reference"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_a_scope_takes_every_kernel_io_thread_off_every_runqueue,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_scope_quiescence_does_not_depend_on_the_freeze_outcome,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_a_held_task_is_not_published_to_a_runqueue,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_releasing_the_hold_republishes_a_held_task,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_the_sweep_takes_a_queued_task_off_the_queue,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_the_inbox_drain_hands_a_held_task_to_the_hold,
    suite = kernel_io
);
slopos_testing::stest!(
    name = test_holding_and_releasing_a_held_task_is_refcount_neutral,
    suite = kernel_io
);
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
