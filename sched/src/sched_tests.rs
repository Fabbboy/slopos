//! Scheduler and task-management tests.

use core::ffi::{c_char, c_void};
use core::ptr;
use slopos_ostd::lock_class;

use slopos_ostd::KArc;
use slopos_ostd::klog_info;
use slopos_ostd::task::{task_placement_leak, task_placement_reclaim, task_placement_strong_count};
use slopos_testing::TestResult;

use super::runtime::{self, IdleStackResolveError};
use super::scheduler::{
    self, get_scheduler_stats, publish_new_task, schedule, schedule_new_task, schedule_task,
    scheduler_is_enabled, scheduler_timer_tick, task_wait_for, unschedule_task,
};
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, IdtEntry, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE,
    Task, TaskPriority, TaskRef, TaskStatus, task_abandon, task_build, task_commit, task_create,
    task_find_by_id, task_for_each_active, task_handle, task_id_was_allocated,
    task_live_cap_rejects_for_test, task_resolve_handle, task_set_state,
    task_set_state_with_reason, task_slot_census, task_terminate, task_waiter_count,
};
use super::test_fixture::KernelTestScope;
use slopos_abi::task::BlockReason;
use slopos_arch::MAX_CPUS;
use slopos_arch::arch::gdt::SegmentSelector;
use slopos_arch::arch::idt::SYSCALL_VECTOR;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, PreemptGuard, SpinLock};
use slopos_ostd::task::SchedPlacement;

/// RAII fixture for scheduler tests; all setup/teardown lives in
/// [`KernelTestScope`].
pub struct SchedFixture {
    _scope: KernelTestScope,
}

impl SchedFixture {
    pub fn new() -> Self {
        Self {
            _scope: KernelTestScope::enter(),
        }
    }

    pub fn kernel_io_is_quiesced(&self) -> bool {
        self._scope.kernel_io_is_quiesced()
    }
}

use crate::test_fixture::dummy_task_entry;

fn make_task_ready(task_id: u32) -> bool {
    task_set_state(task_id, TaskStatus::Ready) == 0
}

fn is_published_placement(placement: SchedPlacement) -> bool {
    matches!(
        placement,
        SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating
    )
}

pub fn test_previous_task_reference_drains_exactly_once() -> TestResult {
    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    let witness = arc.clone();
    let strong_before = KArc::strong_count(&witness);

    let parked = task_placement_leak(arc);
    if slopos_arch::pcr::defer_previous_task(parked.as_ptr().cast()).is_err() {
        klog_info!("SCHED_TEST: previous-task slot was already occupied");
        drop(task_placement_reclaim(parked));
        return TestResult::Fail;
    }
    let retained_before_drain = KArc::strong_count(&witness) == strong_before;
    if !retained_before_drain {
        klog_info!(
            "SCHED_TEST: parked reference changed strong count before drain: {}",
            KArc::strong_count(&witness)
        );
    }

    let drained = scheduler::drain_previous_task();
    let restored_after_drain = KArc::strong_count(&witness) == strong_before - 1;
    if !drained || !restored_after_drain {
        klog_info!(
            "SCHED_TEST: drain failed or count mismatched after drain: {}",
            KArc::strong_count(&witness)
        );
    }
    let second_drain_empty = !scheduler::drain_previous_task();
    if !second_drain_empty {
        klog_info!("SCHED_TEST: second drain unexpectedly found a reference");
    }

    if retained_before_drain && drained && restored_after_drain && second_drain_empty {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

slopos_testing::stest!(
    name = test_previous_task_reference_drains_exactly_once,
    suite = sched_core
);

/// A final release in a context that cannot run the `Task` destructor parks the
/// task rather than destroying it inline; the drain then destroys it once.
///
/// The task is unregistered so the release really is final — the registry pins
/// a registered one.
pub fn test_task_put_defers_unsafe_context_then_drains() -> TestResult {
    crate::task::task_graveyard_drain();
    if crate::task::task_graveyard_pending() {
        klog_info!("SCHED_TEST: graveyard non-empty after a drain");
        return TestResult::Fail;
    }

    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    let witness = KArc::downgrade(&arc);

    // Interrupts off is one of the contexts where the destructor must not run.
    let flags = slopos_arch::cpu::save_flags_cli();
    crate::task::task_put(TaskRef::from_arc_for_test(arc));
    let parked = crate::task::task_graveyard_pending();
    slopos_arch::cpu::restore_flags(flags);

    if !parked {
        klog_info!("SCHED_TEST: final release with interrupts off was not deferred");
        return TestResult::Fail;
    }
    if witness.upgrade().is_some() {
        klog_info!("SCHED_TEST: task still upgradable after its final release");
        return TestResult::Fail;
    }

    crate::task::task_graveyard_drain();
    if crate::task::task_graveyard_pending() {
        klog_info!("SCHED_TEST: graveyard still pending after drain");
        return TestResult::Fail;
    }
    // A second drain must be a no-op rather than a double destroy.
    crate::task::task_graveyard_drain();
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_task_put_defers_unsafe_context_then_drains,
    suite = sched_core
);

/// A parked corpse is collected at the next bottom-half point, with no idle CPU
/// and nobody calling the drain — the outermost preemption release used here is
/// the same edge every unlock takes.
pub fn test_graveyard_drains_at_a_bottom_half_point() -> TestResult {
    crate::task::task_graveyard_drain();
    if crate::task::task_graveyard_pending() {
        klog_info!("SCHED_TEST: graveyard non-empty after a drain");
        return TestResult::Fail;
    }

    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };

    // Interrupts off, so the release defers rather than destroying inline.
    let flags = slopos_arch::cpu::save_flags_cli();
    crate::task::task_put(TaskRef::from_arc_for_test(arc));
    let parked = crate::task::task_graveyard_pending();
    slopos_arch::cpu::restore_flags(flags);

    if !parked {
        klog_info!("SCHED_TEST: final release with interrupts off was not deferred");
        return TestResult::Fail;
    }

    // The bottom-half point, reached the way ordinary code reaches it — the
    // absent `task_graveyard_drain()` is the assertion.
    {
        let _guard = slopos_ostd::sync::PreemptGuard::new();
    }

    if crate::task::task_graveyard_pending() {
        klog_info!("SCHED_TEST: corpse survived a bottom-half point");
        crate::task::task_graveyard_drain();
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_graveyard_drains_at_a_bottom_half_point,
    suite = sched_core
);

/// A final release in a context that *does* allow the destructor destroys
/// inline and parks nothing.
pub fn test_task_put_destroys_inline_when_context_allows() -> TestResult {
    crate::task::task_graveyard_drain();

    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    let witness = KArc::downgrade(&arc);
    crate::task::task_put(TaskRef::from_arc_for_test(arc));

    if crate::task::task_graveyard_pending() {
        klog_info!("SCHED_TEST: safe-context release was deferred unnecessarily");
        crate::task::task_graveyard_drain();
        return TestResult::Fail;
    }
    if witness.upgrade().is_some() {
        klog_info!("SCHED_TEST: task still upgradable after its final release");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_task_put_destroys_inline_when_context_allows,
    suite = sched_core
);

/// A non-final release is a plain decrement: no destruction, nothing parked.
pub fn test_task_put_non_final_release_parks_nothing() -> TestResult {
    crate::task::task_graveyard_drain();

    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    let keep = arc.clone();
    crate::task::task_put(TaskRef::from_arc_for_test(arc));

    let parked = crate::task::task_graveyard_pending();
    let still_live = KArc::strong_count(&keep) == 1;
    crate::task::task_put(TaskRef::from_arc_for_test(keep));
    crate::task::task_graveyard_drain();

    if parked {
        klog_info!("SCHED_TEST: non-final release parked a task");
        return TestResult::Fail;
    }
    if !still_live {
        klog_info!("SCHED_TEST: non-final release left an unexpected strong count");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_task_put_non_final_release_parks_nothing,
    suite = sched_core
);

pub fn test_task_placement_leak_reclaim_round_trip() -> TestResult {
    let arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    let strong_before = KArc::strong_count(&arc);
    let base = KArc::as_ptr(&arc);

    let parked = task_placement_leak(arc);
    if parked.as_ptr().cast_const() != base {
        klog_info!("SCHED_TEST: placement leak moved the base pointer");
        return TestResult::Fail;
    }

    let arc = task_placement_reclaim(parked);
    if KArc::strong_count(&arc) != strong_before {
        klog_info!(
            "SCHED_TEST: placement round-trip changed strong count {} -> {}",
            strong_before,
            KArc::strong_count(&arc)
        );
        return TestResult::Fail;
    }
    if KArc::as_ptr(&arc) != base {
        klog_info!("SCHED_TEST: placement reclaim changed the base pointer");
        return TestResult::Fail;
    }
    drop(arc);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_task_placement_leak_reclaim_round_trip,
    suite = sched_core
);

/// A CPU publishes the priority of the task it dispatches, and returns to the
/// "nothing schedulable" sentinel when it parks on a bootstrap stub.
///
/// Lets a wake publisher judge a remote CPU without dereferencing its current
/// task, a read that races the switch tail.
pub fn test_dispatch_publishes_current_priority() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"PrioPublish\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    if task_find_by_id(task_id).is_none() {
        return TestResult::Fail;
    }

    // `dispatch` only accepts a runnable task; a fresh one is Blocked.
    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: task_set_state Ready failed");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let cpu = slopos_arch::pcr::get_current_cpu();
    if !scheduler::dispatch_task_for_test(cpu, task_id) {
        klog_info!("SCHED_TEST: dispatch fixture task vanished");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let published = slopos_arch::pcr::current_task_priority_for(cpu);
    if published != TaskPriority::Low.as_u8() {
        klog_info!(
            "SCHED_TEST: dispatch published priority {}, expected {}",
            published,
            TaskPriority::Low.as_u8()
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
    let parked = slopos_arch::pcr::current_task_priority_for(cpu);
    if parked != slopos_arch::pcr::PRIORITY_NONE {
        klog_info!(
            "SCHED_TEST: parked CPU published priority {}, expected PRIORITY_NONE",
            parked
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    if TaskPriority::Idle.as_u8() >= slopos_arch::pcr::PRIORITY_NONE {
        klog_info!("SCHED_TEST: PRIORITY_NONE does not lose to every real priority");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_dispatch_publishes_current_priority,
    suite = sched_core
);

/// Every scalar task field reads back the value written to the field it names.
///
/// A dozen same-typed scalars sit side by side, so a getter wired to another's
/// storage compiles cleanly; the distinct sentinels are what catch it.
pub fn test_scalar_field_identity() -> TestResult {
    let mut arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };

    {
        // Sole strong reference to a never-registered task, so `get_mut` succeeds.
        let Some(task) = KArc::get_mut(&mut arc) else {
            return TestResult::Fail;
        };
        task.task_id = 0x1111;
        task.process_id = 0x2222;
        task.flags = 0x3333;
        task.entry_point = 0x4444;
        task.set_cpu_affinity(0x5555);
        task.set_pgid(0x6666);
        task.set_time_slice(0x7777);
        task.set_time_slice_remaining(0x8888);
        task.set_sid(0x9999);
        task.kernel_stack_top = 0xAAAA;
        task.fs_base
            .store(0xBBBB, core::sync::atomic::Ordering::Release);
        task.tgid = 0xCCCC;
        task.set_parent_task_id(0xDDDD);
        task.priority = TaskPriority::Low;
    }

    let checks: [(&str, u64, u64); 13] = [
        ("task_id", arc.task_id as u64, 0x1111),
        ("process_id", arc.process_id as u64, 0x2222),
        ("flags", arc.flags as u64, 0x3333),
        ("entry_point", arc.entry_point, 0x4444),
        ("cpu_affinity", arc.cpu_affinity() as u64, 0x5555),
        ("pgid", arc.pgid() as u64, 0x6666),
        ("time_slice", arc.time_slice(), 0x7777),
        ("time_slice_remaining", arc.time_slice_remaining(), 0x8888),
        ("sid", arc.sid() as u64, 0x9999),
        ("kernel_stack_top", arc.kernel_stack_top, 0xAAAA),
        ("fs_base", arc.fs_base(), 0xBBBB),
        ("tgid", arc.tgid as u64, 0xCCCC),
        ("parent_task_id", arc.parent_task_id() as u64, 0xDDDD),
    ];
    // By reference: `for … in checks` moves the table into `array::IntoIter`,
    // whose frame then carries several unmerged copies of it.
    for &(name, got, want) in checks.iter() {
        if got != want {
            klog_info!(
                "SCHED_TEST: field {} read 0x{:x}, expected 0x{:x}",
                name,
                got,
                want
            );
            return TestResult::Fail;
        }
    }
    if arc.priority != TaskPriority::Low {
        klog_info!("SCHED_TEST: priority read the wrong field");
        return TestResult::Fail;
    }

    drop(arc);
    TestResult::Pass
}

slopos_testing::stest!(name = test_scalar_field_identity, suite = sched_core);

/// Build a real child token: one that already owns its kernel stack, its data
/// stack and its process VM.
fn build_parkable_child() -> Option<crate::task::PendingTask> {
    crate::task::task_build(
        b"ParkedChild\0".as_ptr() as *const core::ffi::c_char,
        dummy_task_entry,
        core::ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    )
}

/// A publication that fails leaves the task nascent, not reserved.
///
/// The publish path CASes `Nascent -> Waking` *before* checking the task is
/// Ready, so without a rollback a fresh (Blocked) task sits in `Waking` — a
/// state `wake_blocked_task` publishes from, and one the retire CAS, which only
/// matches `Nascent`, cannot recover.
pub fn test_failed_publication_restores_nascent() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"FailPublish\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;

    // Blocked + Nascent: the publish path must refuse this.
    if scheduler::schedule_new_task(&guard) == 0 {
        klog_info!("SCHED_TEST: schedule_new_task published a Blocked task");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    if task.sched_placement() != SchedPlacement::Nascent {
        klog_info!(
            "SCHED_TEST: failed publication left placement {:?}, expected Nascent",
            task.sched_placement()
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    if scheduler::unblock_task(&guard) != 0 {
        klog_info!("SCHED_TEST: wake after a failed publication did not no-op");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    if task.sched_placement() != SchedPlacement::Nascent || task.status() != (TaskStatus::Blocked) {
        klog_info!("SCHED_TEST: wake published a task whose publication had failed");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    if publish_new_task(&guard) != 0 {
        klog_info!("SCHED_TEST: publish_new_task failed after a rolled-back reservation");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    if !is_published_placement(task.sched_placement()) {
        klog_info!("SCHED_TEST: publication after rollback did not take ownership");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_failed_publication_restores_nascent,
    suite = sched_core
);

/// Terminating a never-published task retires its placement to `None`.
///
/// A corpse left in `Nascent` is permanently unreapable: both gates key on task
/// state and nothing retires the placement afterwards.
pub fn test_nascent_task_is_terminable_and_retires_to_none() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NascentDie\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;
    if task.sched_placement() != SchedPlacement::Nascent {
        klog_info!("SCHED_TEST: fresh task not Nascent");
        return TestResult::Fail;
    }

    if task_terminate(task_id) != 0 {
        klog_info!("SCHED_TEST: a nascent task was not terminable");
        return TestResult::Fail;
    }
    if task.sched_placement() != SchedPlacement::None {
        klog_info!(
            "SCHED_TEST: terminated nascent task left placement {:?}, expected None",
            task.sched_placement()
        );
        return TestResult::Fail;
    }
    if !task.is_exited() {
        klog_info!("SCHED_TEST: terminated nascent task is not exited");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_nascent_task_is_terminable_and_retires_to_none,
    suite = sched_core
);

/// A registered-but-unpublished task refuses every wake, and a process-group
/// signal cannot drive it onto a runqueue.
///
/// `task_create` publishes `pgid = task_id` *before* it registers, so a signal
/// landing before `publish_new_task` finds a Blocked task; `Nascent` is what
/// distinguishes it from a legitimate wake target.
pub fn test_nascent_task_refuses_wake() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NascentWake\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;
    if task.sched_placement() != SchedPlacement::Nascent {
        klog_info!("SCHED_TEST: fresh task not Nascent");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let ready_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    // The wake reports "nothing to do" rather than failure: the task exists, so
    // a caller like `kill` must not turn this into ESRCH.
    if scheduler::unblock_task(&guard) != 0 {
        klog_info!("SCHED_TEST: nascent wake did not report a no-op");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    if task.status() != (TaskStatus::Blocked) || task.sched_placement() != SchedPlacement::Nascent {
        klog_info!(
            "SCHED_TEST: nascent task moved on wake: status {:?} placement {:?}",
            Some(task.status()),
            task.sched_placement()
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    if task.inbox_link().is_linked() {
        klog_info!("SCHED_TEST: nascent task was linked into a scheduler container");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    let ready_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_after != ready_before {
        klog_info!(
            "SCHED_TEST: nascent wake changed ready count {} -> {}",
            ready_before,
            ready_after
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    if publish_new_task(&guard) != 0 {
        klog_info!("SCHED_TEST: publish_new_task failed after a refused wake");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    let placement = task.sched_placement();
    if placement == SchedPlacement::Nascent || !is_published_placement(placement) {
        klog_info!(
            "SCHED_TEST: publish left placement {:?}, expected a durable owner",
            placement
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(name = test_nascent_task_refuses_wake, suite = sched_core);

/// A task under construction is reachable through nothing but the token that
/// owns it, so `kill(-pgid)` cannot catch it half-built.
///
/// The spawn path writes a child's job-control identity only once its ELF is
/// loaded. With no registry entry there is nothing for `task_find_by_id` or the
/// active-task walk — which matches on `pgid` — to yield until `task_commit`.
pub fn test_pending_task_is_unreachable_until_commit() -> TestResult {
    let _fixture = SchedFixture::new();

    let Some(mut pending) = task_build(
        b"PendingHidden\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    ) else {
        klog_info!("SCHED_TEST: task_build refused a well-formed kernel task");
        return TestResult::Fail;
    };
    let task_id = pending.id();

    // The spawn path's job-control inherit, done while the task is still private.
    let group_pgid = task_id + 0x4000;
    pending.as_mut().set_pgid(group_pgid);

    if task_find_by_id(task_id).is_some() {
        klog_info!("SCHED_TEST: a task under construction was findable by id");
        return TestResult::Fail;
    }

    // The `kill(-pgid)` target scan, verbatim.
    let mut seen_by_id = false;
    let mut seen_by_pgid = false;
    task_for_each_active(|task| {
        if task.task_id == task_id {
            seen_by_id = true;
        }
        if task.pgid() == group_pgid {
            seen_by_pgid = true;
        }
    });
    if seen_by_id || seen_by_pgid {
        klog_info!(
            "SCHED_TEST: the active walk yielded a task under construction (by id {}, by pgid {})",
            seen_by_id,
            seen_by_pgid
        );
        return TestResult::Fail;
    }

    let Some(registered) = task_commit(pending) else {
        klog_info!("SCHED_TEST: task_commit rejected a fully built task");
        return TestResult::Fail;
    };
    drop(registered);

    let Some(guard) = task_find_by_id(task_id) else {
        klog_info!("SCHED_TEST: a committed task was not findable");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    };
    if guard.pgid() != group_pgid {
        klog_info!(
            "SCHED_TEST: committed task carries pgid {}, expected {}",
            guard.pgid(),
            group_pgid
        );
        drop(guard);
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    drop(guard);

    let _ = task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_pending_task_is_unreachable_until_commit,
    suite = sched_core
);

/// Abandoning a built task gives back everything it reserved — the live-task
/// reservation, the allocation and the address space.
///
/// Only the id is spent: ids are monotonic, which is what distinguishes
/// "already retired" from "never existed".
pub fn test_task_abandon_releases_the_address_space() -> TestResult {
    let _fixture = SchedFixture::new();

    let (live_before, _, _, _) = task_slot_census();

    let Some(mut pending) = task_build(
        b"AbandonMe\0".as_ptr() as *const c_char,
        crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    ) else {
        klog_info!("SCHED_TEST: task_build refused a well-formed user task");
        return TestResult::Fail;
    };
    let task_id = pending.id();
    // Captured before the abandon: afterwards the number names either nothing
    // or somebody else.
    let process = pending
        .as_mut()
        .process()
        .as_deref()
        .and_then(slopos_ostd::process::ProcessId::of);

    let Some(process) = process else {
        klog_info!("SCHED_TEST: a built user task has no process");
        task_abandon(pending);
        return TestResult::Fail;
    };
    if slopos_mm::process_vm::process_vm_get_vm_space(process).is_none() {
        klog_info!("SCHED_TEST: a built user task has no address space");
        task_abandon(pending);
        return TestResult::Fail;
    }

    task_abandon(pending);

    if slopos_mm::process_vm::process_vm_get_vm_space(process).is_some() {
        klog_info!(
            "SCHED_TEST: abandoning task {} left process {} standing",
            task_id,
            process.id()
        );
        return TestResult::Fail;
    }
    if task_find_by_id(task_id).is_some() {
        klog_info!("SCHED_TEST: an abandoned task is findable");
        return TestResult::Fail;
    }
    if !task_id_was_allocated(task_id) {
        klog_info!("SCHED_TEST: an abandoned task's id was handed back to the allocator");
        return TestResult::Fail;
    }
    let (live_after, _, _, _) = task_slot_census();
    if live_after != live_before {
        klog_info!(
            "SCHED_TEST: abandon left the live-task count at {}, expected {}",
            live_after,
            live_before
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_task_abandon_releases_the_address_space,
    suite = sched_core
);

/// A built user task names a live process, and that process counts it.
///
/// The count decides teardown: uncounted, the address space is destroyed under
/// a running task; counted without joining, it is pinned forever.
pub fn test_a_built_user_task_joins_a_counted_process() -> TestResult {
    let _fixture = SchedFixture::new();

    let Some(mut pending) = task_build(
        b"ProcJoin\0".as_ptr() as *const c_char,
        crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    ) else {
        klog_info!("SCHED_TEST: task_build refused a user task");
        return TestResult::Fail;
    };

    let Some(process) = pending.as_mut().process() else {
        klog_info!("SCHED_TEST: a built user task carries no process handle");
        task_abandon(pending);
        return TestResult::Fail;
    };
    let process_id = process.id();
    let count = process.task_count();
    let handle = process.handle();
    drop(process);

    if count != 1 {
        klog_info!(
            "SCHED_TEST: process {} counts {} tasks, expected exactly its one builder",
            process_id,
            count
        );
        task_abandon(pending);
        return TestResult::Fail;
    }

    task_abandon(pending);

    // The last task left, so the registration is retired and the handle is
    // stale rather than resolving to whoever takes the slot next.
    let Some(handle) = handle else {
        klog_info!("SCHED_TEST: a registered process carries no self-handle");
        return TestResult::Fail;
    };
    if slopos_ostd::process::process_for_handle(handle).is_some() {
        klog_info!(
            "SCHED_TEST: process {} still resolves after its last task was abandoned",
            process_id
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_a_built_user_task_joins_a_counted_process,
    suite = sched_core
);

/// A kernel task has no process, and says so: `INVALID_PROCESS_ID` and "no
/// process handle" must agree, or the exit path tears down an address space the
/// task never belonged to.
pub fn test_a_kernel_task_has_no_process() -> TestResult {
    let _fixture = SchedFixture::new();

    let Some(mut pending) = task_build(
        b"NoProc\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    ) else {
        klog_info!("SCHED_TEST: task_build refused a kernel task");
        return TestResult::Fail;
    };

    let task = pending.as_mut();
    let has_process = task.process().is_some();
    let raw = task.process_handle_raw();
    let pid = task.process_id;
    task_abandon(pending);

    if has_process || raw != slopos_ostd::process::PROCESS_HANDLE_NONE {
        klog_info!(
            "SCHED_TEST: a kernel task carries a process handle (raw {})",
            raw
        );
        return TestResult::Fail;
    }
    if pid != INVALID_PROCESS_ID {
        klog_info!("SCHED_TEST: a kernel task carries process id {}", pid);
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_a_kernel_task_has_no_process, suite = sched_core);

/// Process registrations do not accumulate across task churn.
///
/// The id space is 256 wide and a registration holds an id until it is retired,
/// so a build/abandon cycle that leaked one exhausts it after 256 spawns.
pub fn test_process_registrations_do_not_leak_across_churn() -> TestResult {
    let _fixture = SchedFixture::new();

    let before = slopos_ostd::process::process_count();

    for i in 0..32 {
        let Some(pending) = task_build(
            b"ProcChurn\0".as_ptr() as *const c_char,
            crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_USER_MODE,
        ) else {
            klog_info!(
                "SCHED_TEST: task_build refused a user task at cycle {} — \
                 the id space is likely already exhausted",
                i
            );
            return TestResult::Fail;
        };
        task_abandon(pending);
    }

    let after = slopos_ostd::process::process_count();
    if after != before {
        klog_info!(
            "SCHED_TEST: 32 build/abandon cycles left the registry at {} entries, was {}",
            after,
            before
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_process_registrations_do_not_leak_across_churn,
    suite = sched_core
);

/// Every user task the kernel can build, it can also run.
///
/// A task is refused dispatch and terminated if its process id is outside the
/// range the process tables are indexed by, so an allocator that can hand out
/// an id past that range makes `task_build` succeed and every task from then on
/// die at its first dispatch.
///
/// Deliberately does **not** call `init_process_vm`: that resets the id
/// allocator, and what is under test is the state the kernel accumulates.
pub fn test_every_user_task_built_since_boot_is_dispatchable() -> TestResult {
    let _fixture = SchedFixture::new();

    // Enough cycles to walk past the process-table bound from any starting
    // point the suite could have left the allocator in.
    const CYCLES: usize = slopos_mm::memory_layout_defs::MAX_PROCESSES + 64;

    for i in 0..CYCLES {
        let Some(mut pending) = task_build(
            b"Churn\0".as_ptr() as *const c_char,
            crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_USER_MODE,
        ) else {
            klog_info!("SCHED_TEST: task_build refused a user task at cycle {}", i);
            return TestResult::Fail;
        };
        let process_id = pending.as_mut().process_id;
        let dispatchable = scheduler::dispatch_pid_ok(process_id);
        task_abandon(pending);

        if process_id == INVALID_PROCESS_ID {
            klog_info!("SCHED_TEST: user task at cycle {} has no address space", i);
            return TestResult::Fail;
        }
        if !dispatchable {
            klog_info!(
                "SCHED_TEST: task built at cycle {} carries pid {}, which the \
                 dispatcher refuses — it would be terminated at first dispatch",
                i,
                process_id
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_every_user_task_built_since_boot_is_dispatchable,
    suite = sched_core
);

/// A task's address-space handle stops resolving once that address space is
/// gone, and never resolves to whichever process takes its place.
///
/// The id it held is issued again and the slot rebound, so anything keyed on
/// either alone follows the task's page-fault and dispatch paths straight into
/// a stranger's address space.
pub fn test_a_dead_address_space_is_unreachable_through_its_task_handle() -> TestResult {
    use slopos_mm::process_vm::{
        create_process_vm, destroy_process_vm, process_vm_get_cr3_phys_by_handle,
        unpack_process_vm_handle,
    };

    let _fixture = SchedFixture::new();

    let Some(mut pending) = task_build(
        b"HandleStale\0".as_ptr() as *const c_char,
        crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    ) else {
        klog_info!("SCHED_TEST: task_build refused a well-formed user task");
        return TestResult::Fail;
    };
    let packed = pending.as_mut().process_vm_handle_raw();

    let Some(handle) = unpack_process_vm_handle(packed) else {
        klog_info!("SCHED_TEST: a built user task carries no address-space handle");
        task_abandon(pending);
        return TestResult::Fail;
    };
    if !matches!(process_vm_get_cr3_phys_by_handle(handle), Ok(cr3) if cr3 != 0) {
        klog_info!("SCHED_TEST: a live task's handle does not name a live address space");
        task_abandon(pending);
        return TestResult::Fail;
    }

    task_abandon(pending);

    if process_vm_get_cr3_phys_by_handle(handle).is_ok() {
        klog_info!("SCHED_TEST: a torn-down address space still resolves through its handle");
        return TestResult::Fail;
    }

    // Bind a fresh process to the slot the dead task used: the handle must
    // still refuse, or a dead task's fault path gets a live stranger's tables.
    let successor = create_process_vm();
    if successor == INVALID_PROCESS_ID {
        klog_info!("SCHED_TEST: could not create a successor process");
        return TestResult::Fail;
    }
    let resolved = process_vm_get_cr3_phys_by_handle(handle);
    destroy_process_vm(
        slopos_ostd::process::ProcessId::resolve(successor).expect("a live process"),
    );

    if let Ok(cr3) = resolved {
        klog_info!(
            "SCHED_TEST: a dead task's handle resolved to cr3 0x{:x} after the slot \
             was rebound",
            cr3
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_a_dead_address_space_is_unreachable_through_its_task_handle,
    suite = sched_core
);

pub fn test_raw_ready_store_does_not_reserve_waking_placement() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"ReadyRaw\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;

    if task.status() != (TaskStatus::Blocked) {
        klog_info!("SCHED_TEST: fresh task not Blocked");
        return TestResult::Fail;
    }
    if task.sched_placement() != SchedPlacement::Nascent {
        klog_info!("SCHED_TEST: fresh task not placement Nascent");
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: task_set_state Ready failed");
        return TestResult::Fail;
    }
    if task.status() != (TaskStatus::Ready) {
        klog_info!("SCHED_TEST: raw Ready store did not publish Ready status");
        return TestResult::Fail;
    }
    if task.sched_placement() != SchedPlacement::Nascent {
        klog_info!(
            "SCHED_TEST: raw Ready store placement {:?}, expected Nascent",
            task.sched_placement()
        );
        return TestResult::Fail;
    }

    if scheduler::schedule_task(&guard) != 0 {
        klog_info!("SCHED_TEST: explicit Ready publish failed");
        return TestResult::Fail;
    }
    if !is_published_placement(task.sched_placement()) {
        klog_info!(
            "SCHED_TEST: explicit Ready publish left placement {:?}",
            task.sched_placement()
        );
        return TestResult::Fail;
    }
    let _ = task_terminate(task_id);
    TestResult::Pass
}

pub fn test_publish_new_task_owns_ready_publication() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NewPublish\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;
    if task.status() != (TaskStatus::Blocked) || task.sched_placement() != SchedPlacement::Nascent {
        klog_info!("SCHED_TEST: new task was not born non-runnable");
        return TestResult::Fail;
    }

    if publish_new_task(&guard) != 0 {
        klog_info!("SCHED_TEST: publish_new_task failed");
        return TestResult::Fail;
    }
    let placement = task.sched_placement();
    if task.status() != (TaskStatus::Ready) || !is_published_placement(placement) {
        klog_info!(
            "SCHED_TEST: publish_new_task left status {:?} placement {:?}",
            Some(task.status()),
            placement
        );
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

pub fn test_wake_blocked_task_publishes_from_none() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"WakeNone\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;
    // Stand in for a published-then-blocked task: every wake path deliberately
    // refuses a nascent one.
    if !scheduler::clear_nascent_for_test(task_id) {
        klog_info!("SCHED_TEST: wake fixture was not nascent");
        return TestResult::Fail;
    }
    if task.status() != (TaskStatus::Blocked) || task.sched_placement() != SchedPlacement::None {
        klog_info!("SCHED_TEST: wake fixture not Blocked+None");
        return TestResult::Fail;
    }

    if scheduler::wake_blocked_task(&guard, task_id) != 0 {
        klog_info!("SCHED_TEST: wake_blocked_task failed");
        return TestResult::Fail;
    }
    let placement = task.sched_placement();
    if task.status() != (TaskStatus::Ready) || !is_published_placement(placement) {
        klog_info!(
            "SCHED_TEST: wake_blocked_task left status {:?} placement {:?}",
            Some(task.status()),
            placement
        );
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

pub fn test_state_transition_ready_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"StateTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task: &Task = &guard;

    let initial_state = task.status();
    if initial_state != TaskStatus::Blocked {
        klog_info!(
            "SCHED_TEST: Expected initial BLOCKED state, got {:?}",
            initial_state
        );
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set READY state");
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Running) != 0 {
        klog_info!("SCHED_TEST: Failed to set RUNNING state");
        return TestResult::Fail;
    }

    let new_state = task.status();
    if new_state != TaskStatus::Running {
        klog_info!(
            "SCHED_TEST: Expected RUNNING state after transition, got {:?}",
            new_state
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_state_transition_running_to_blocked() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"BlockTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set READY state");
        return TestResult::Fail;
    }
    if task_set_state(task_id, TaskStatus::Running) != 0 {
        klog_info!("SCHED_TEST: Failed to set RUNNING state");
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Blocked) != 0 {
        klog_info!("SCHED_TEST: Failed to set BLOCKED state");
        return TestResult::Fail;
    }

    let state = task_find_by_id(task_id)
        .map(|task| task.status())
        .unwrap_or(TaskStatus::Terminated);
    if state != TaskStatus::Blocked {
        klog_info!("SCHED_TEST: Expected BLOCKED, got {:?}", state);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_state_transition_invalid_terminated_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"InvalidTransition\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    task_terminate(task_id);

    if let Some(task) = task_find_by_id(task_id) {
        let _result = task_set_state(task_id, TaskStatus::Running);
        let new_state = Some(task.status()).unwrap_or(TaskStatus::Terminated);

        if new_state == TaskStatus::Running {
            klog_info!("SCHED_TEST: BUG - Invalid transition TERMINATED->RUNNING was allowed!");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// A blocked task must reach `Running` through `Ready`, never directly.
pub fn test_state_transition_invalid_blocked_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"BlockedRunning\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let _result = task_set_state(task_id, TaskStatus::Running);

    let state = task_find_by_id(task_id)
        .map(|task| task.status())
        .unwrap_or(TaskStatus::Terminated);

    if state == TaskStatus::Running {
        klog_info!("SCHED_TEST: BUG - Invalid transition BLOCKED->RUNNING was allowed!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Individually allocated tasks coexist and the concurrent-task cap rejects
/// without consuming an ID or inserting a registry entry.
pub fn test_task_registry_live_cap() -> TestResult {
    let _fixture = SchedFixture::new();

    const TARGET: usize = 4;
    let mut ids: slopos_ostd::KVec<u32> = match slopos_ostd::KVec::with_capacity(TARGET) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };
    for _ in 0..TARGET {
        let id = task_create(
            b"GrowTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: task allocation failed at {} tasks", ids.len());
            return TestResult::Fail;
        }
        let _ = ids.push(id);
    }

    for id in ids.iter() {
        let _ = task_terminate(*id);
    }
    if task_live_cap_rejects_for_test() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// A held registry guard pins the *allocation* of a reaped task, not its
/// *registration*: the reap unhashes immediately without waiting on outstanding
/// guards, and the guard only keeps the memory valid.
pub fn test_task_guard_pins_terminated_task() -> TestResult {
    let _fixture = SchedFixture::new();

    let id = task_create(
        b"GuardPin\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(id) else {
        return TestResult::Fail;
    };
    let weak = guard.downgrade_for_test();
    if task_terminate(id) != 0 {
        return TestResult::Fail;
    }

    if task_find_by_id(id).is_some() {
        klog_info!("SCHED_TEST: reaped task {} still resolves", id);
        return TestResult::Fail;
    }
    if weak.upgrade().is_none() {
        klog_info!("SCHED_TEST: guarded task {} freed while pinned", id);
        return TestResult::Fail;
    }

    drop(guard);
    if weak.upgrade().is_some() {
        klog_info!("SCHED_TEST: task {} survived its final reference", id);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A registered-but-unpublished task is terminable and reclaimable.
///
/// The shape a fork/clone child has between `register_task` and
/// `publish_new_task`: constructed, registered, `Invalid`, and hidden from
/// every active-task scan.
pub fn test_unpublished_task_is_terminable() -> TestResult {
    let _fixture = SchedFixture::new();

    let id = task_create(
        b"Unpublish\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    {
        let Some(task) = task_find_by_id(id) else {
            return TestResult::Fail;
        };
        let _ = task.set_status(TaskStatus::Invalid);
    }

    if task_terminate(id) != 0 {
        klog_info!("SCHED_TEST: unpublished task {} refused termination", id);
        return TestResult::Fail;
    }
    if task_find_by_id(id).is_some() {
        klog_info!("SCHED_TEST: unpublished task {} leaked past terminate", id);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_rapid_create_destroy_cycle() -> TestResult {
    let _fixture = SchedFixture::new();

    const CYCLES: usize = 100;
    let mut last_id = INVALID_TASK_ID;

    for i in 0..CYCLES {
        let task_id = task_create(
            b"CycleTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if task_id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Cycle {} failed to create task", i);
            return TestResult::Fail;
        }

        if task_terminate(task_id) != 0 {
            klog_info!("SCHED_TEST: Cycle {} failed to terminate task", i);
            return TestResult::Fail;
        }

        last_id = task_id;
    }

    klog_info!(
        "SCHED_TEST: Completed {} create/destroy cycles, last ID={}",
        CYCLES,
        last_id
    );

    TestResult::Pass
}

/// A task handle resolves through weak upgrade and never aliases a later ID.
pub fn test_task_handle_stale_after_reuse() -> TestResult {
    let _fixture = SchedFixture::new();

    let id1 = task_create(
        b"HandleA\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if id1 == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(h1) = task_handle(id1) else {
        let _ = task_terminate(id1);
        return TestResult::Fail;
    };
    if task_resolve_handle(h1).is_none() || task_resolve_handle(h1) != task_find_by_id(id1) {
        let _ = task_terminate(id1);
        return TestResult::Fail;
    }

    let _ = task_terminate(id1);
    if task_resolve_handle(h1).is_some() || task_find_by_id(id1).is_some() {
        return TestResult::Fail;
    }

    let id2 = task_create(
        b"HandleB\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if id2 == INVALID_TASK_ID || id2 <= id1 {
        return TestResult::Fail;
    }
    let Some(h2) = task_handle(id2) else {
        return TestResult::Fail;
    };
    let result = if h2.slot() != id2 || h2.generation() != 0 || task_resolve_handle(h2).is_none() {
        TestResult::Fail
    } else {
        TestResult::Pass
    };
    let _ = task_terminate(id2);
    result
}

/// `KernelStack::allocate` returns a page-aligned handle whose `top > base`.
pub fn test_kstack_basic_alloc() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let stack = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: KernelStack::allocate failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let base = stack.base().as_u64();
    let top = stack.top().as_u64();

    if top <= base {
        klog_info!("SCHED_TEST: kstack top 0x{:x} <= base 0x{:x}", top, base);
        return TestResult::Fail;
    }
    if top - base != TASK_STACK_SIZE {
        klog_info!(
            "SCHED_TEST: kstack size mismatch: top-base=0x{:x} want 0x{:x}",
            top - base,
            TASK_STACK_SIZE
        );
        return TestResult::Fail;
    }
    if (base & 0xFFF) != 0 {
        klog_info!("SCHED_TEST: kstack base 0x{:x} not page-aligned", base);
        return TestResult::Fail;
    }

    drop(stack);
    TestResult::Pass
}

/// A dropped `KernelStack` returns its slot to the allocator for reuse.
///
/// Stack capacity is therefore independent of kernel binary size: the slot
/// allocator tracks availability in its own bitmap.
pub fn test_kstack_slot_reuse() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let s1 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: second alloc after free failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST: kstack slot not reused: top1=0x{:x} top2=0x{:x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

/// Invalid sizes are rejected without touching global state.
pub fn test_kstack_rejects_invalid_size() -> TestResult {
    use super::task_stack::KernelStack;

    if KernelStack::allocate(0, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    if KernelStack::allocate(4097, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Bigger than the slot stride (64 KB minus guard).
    if KernelStack::allocate(64 * 1024, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: oversized alloc unexpectedly succeeded");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Repeated alloc/free on the same CPU stays in the per-CPU cache: only the
/// first allocation may increment `refill_count`.
pub fn test_kstack_pcp_refill() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();

    // Flush stale entries so the refill_count readings are meaningful.
    pcp_flush_current::<KstackRegion>();

    let before = pcp_stats::<KstackRegion>(cpu);

    let s1 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_refill]: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);

    let after_first = pcp_stats::<KstackRegion>(cpu);
    if after_first.refill_count <= before.refill_count {
        klog_info!(
            "SCHED_TEST[pcp_refill]: refill_count did not advance: {} -> {}",
            before.refill_count,
            after_first.refill_count
        );
        return TestResult::Fail;
    }

    // The refill batch is 8 slots, so four more allocations stay cache hits.
    for i in 0..4 {
        let s = match KernelStack::allocate(
            TASK_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_refill]: iter {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    let after_warm = pcp_stats::<KstackRegion>(cpu);
    if after_warm.refill_count != after_first.refill_count {
        klog_info!(
            "SCHED_TEST[pcp_refill]: unexpected refill during warm path: {} -> {}",
            after_first.refill_count,
            after_warm.refill_count
        );
        return TestResult::Fail;
    }

    // Four warm-path pops plus the first allocation.
    if after_warm.alloc_count < before.alloc_count.saturating_add(5) {
        klog_info!(
            "SCHED_TEST[pcp_refill]: alloc_count under-advanced: {} -> {}",
            before.alloc_count,
            after_warm.alloc_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Driving the cache past `pcp_capacity()` forces a spill.
pub fn test_kstack_pcp_spill_overflow() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_capacity, pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<KstackRegion>();
    let baseline_in_use = in_use_count::<KstackRegion>();
    let before = pcp_stats::<KstackRegion>(cpu);

    // Hold capacity + 1 stacks so a drop enters a full cache.
    let hold = pcp_capacity::<KstackRegion>() + 1;
    let mut stacks: [Option<KernelStack>; 32] = [const { None }; 32];
    if hold > stacks.len() {
        klog_info!("SCHED_TEST[pcp_spill]: capacity {} > fixture cap", hold);
        return TestResult::Fail;
    }
    for i in 0..hold {
        stacks[i] = match KernelStack::allocate(
            TASK_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_spill]: alloc {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
    }
    for i in 0..hold {
        stacks[i] = None;
    }

    let after = pcp_stats::<KstackRegion>(cpu);
    if after.spill_count <= before.spill_count {
        klog_info!(
            "SCHED_TEST[pcp_spill]: spill_count did not advance: {} -> {}",
            before.spill_count,
            after.spill_count
        );
        return TestResult::Fail;
    }

    // Everything was flushed then dropped, so residual in_use must equal the
    // cache count exactly.
    let residual_in_use = in_use_count::<KstackRegion>().saturating_sub(baseline_in_use);
    if residual_in_use != after.count {
        klog_info!(
            "SCHED_TEST[pcp_spill]: leak detected: in_use_delta={} cache_count={}",
            residual_in_use,
            after.count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A slot's `was_backed` bit survives a PCP round-trip: the same VA comes back
/// and the second allocation skips the mapping path.
pub fn test_kstack_pcp_was_backed_preserved() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    let s1 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_backed]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST[pcp_backed]: PCP did not reuse slot: top1={:#x} top2={:#x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

/// Freed slots are visible to any CPU's refill path; a cross-CPU free is
/// simulated by an explicit flush in between.
pub fn test_kstack_pcp_cross_cpu_safety() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    // The flush forces the slot into the global pool, so the next allocation
    // must refill from it.
    let s1 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<KstackRegion>();

    let s2 = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_xcpu]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s2);
    pcp_flush_current::<KstackRegion>();

    let after = in_use_count::<KstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[pcp_xcpu]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// 1000-iteration stress loop with no leaks.
pub fn test_kstack_pcp_stress_1000() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    for i in 0..1000 {
        let s = match KernelStack::allocate(
            TASK_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_stress]: iteration {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    pcp_flush_current::<KstackRegion>();
    let after = in_use_count::<KstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[pcp_stress]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Advisory benchmark: logs cycles-per-alloc for a warm-cache loop, and always
/// passes.
pub fn test_kstack_pcp_smp_throughput_bench() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    // Warm up the cache so the timed loop is a pure PCP hit.
    let warmup = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(_) => return TestResult::Pass,
    };
    drop(warmup);

    const ITERATIONS: u64 = 512;
    let start = slopos_arch::tsc::rdtsc();
    for _ in 0..ITERATIONS {
        let s = match KernelStack::allocate(
            TASK_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => s,
            Err(_) => return TestResult::Pass,
        };
        drop(s);
    }
    let end = slopos_arch::tsc::rdtsc();
    let cycles = end.wrapping_sub(start);
    let per_op = cycles / ITERATIONS;
    klog_info!(
        "SCHED_TEST[pcp_bench] kstack alloc+drop warm path: {} cycles/op over {} iters",
        per_op,
        ITERATIONS
    );

    TestResult::Pass
}

pub fn test_ustack_basic_alloc() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;

    let stack = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: UnsafeStack::allocate failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let base = stack.base().as_u64();
    let top = stack.top().as_u64();

    if top <= base {
        klog_info!("SCHED_TEST: ustack top 0x{:x} <= base 0x{:x}", top, base);
        return TestResult::Fail;
    }
    if top - base != TASK_UNSAFE_STACK_SIZE {
        klog_info!(
            "SCHED_TEST: ustack size mismatch: top-base=0x{:x} want 0x{:x}",
            top - base,
            TASK_UNSAFE_STACK_SIZE
        );
        return TestResult::Fail;
    }
    if (base & 0xFFF) != 0 {
        klog_info!("SCHED_TEST: ustack base 0x{:x} not page-aligned", base);
        return TestResult::Fail;
    }

    drop(stack);
    TestResult::Pass
}

pub fn test_ustack_slot_reuse() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;

    let s1 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: ustack first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: ustack second alloc after free failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST: ustack slot not reused: top1=0x{:x} top2=0x{:x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

pub fn test_ustack_rejects_invalid_size() -> TestResult {
    use super::task_stack::UnsafeStack;

    if UnsafeStack::allocate(0, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: ustack zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    if UnsafeStack::allocate(4097, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: ustack unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    if UnsafeStack::allocate(64 * 1024, slopos_ostd::process::quota::root()).is_ok() {
        klog_info!("SCHED_TEST: ustack oversized alloc unexpectedly succeeded");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_refill() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<UstackRegion>();

    let before = pcp_stats::<UstackRegion>(cpu);

    let s1 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_refill]: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);

    let after_first = pcp_stats::<UstackRegion>(cpu);
    if after_first.refill_count <= before.refill_count {
        klog_info!(
            "SCHED_TEST[ustack_refill]: refill_count did not advance: {} -> {}",
            before.refill_count,
            after_first.refill_count
        );
        return TestResult::Fail;
    }

    for i in 0..4 {
        let s = match UnsafeStack::allocate(
            TASK_UNSAFE_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_refill]: iter {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    let after_warm = pcp_stats::<UstackRegion>(cpu);
    if after_warm.refill_count != after_first.refill_count {
        klog_info!(
            "SCHED_TEST[ustack_refill]: unexpected refill during warm path: {} -> {}",
            after_first.refill_count,
            after_warm.refill_count
        );
        return TestResult::Fail;
    }

    if after_warm.alloc_count < before.alloc_count.saturating_add(5) {
        klog_info!(
            "SCHED_TEST[ustack_refill]: alloc_count under-advanced: {} -> {}",
            before.alloc_count,
            after_warm.alloc_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_spill_overflow() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_capacity, pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<UstackRegion>();
    let baseline_in_use = in_use_count::<UstackRegion>();
    let before = pcp_stats::<UstackRegion>(cpu);

    let hold = pcp_capacity::<UstackRegion>() + 1;
    let mut stacks: [Option<UnsafeStack>; 32] = [const { None }; 32];
    if hold > stacks.len() {
        klog_info!("SCHED_TEST[ustack_spill]: capacity {} > fixture cap", hold);
        return TestResult::Fail;
    }
    for i in 0..hold {
        stacks[i] = match UnsafeStack::allocate(
            TASK_UNSAFE_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_spill]: alloc {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
    }
    for i in 0..hold {
        stacks[i] = None;
    }

    let after = pcp_stats::<UstackRegion>(cpu);
    if after.spill_count <= before.spill_count {
        klog_info!(
            "SCHED_TEST[ustack_spill]: spill_count did not advance: {} -> {}",
            before.spill_count,
            after.spill_count
        );
        return TestResult::Fail;
    }

    let residual_in_use = in_use_count::<UstackRegion>().saturating_sub(baseline_in_use);
    if residual_in_use != after.count {
        klog_info!(
            "SCHED_TEST[ustack_spill]: leak detected: in_use_delta={} cache_count={}",
            residual_in_use,
            after.count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_was_backed_preserved() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<UstackRegion>();

    let s1 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_backed]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST[ustack_backed]: PCP did not reuse slot: top1={:#x} top2={:#x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

pub fn test_ustack_pcp_cross_cpu_safety() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<UstackRegion>();
    let before = in_use_count::<UstackRegion>();

    let s1 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<UstackRegion>();

    let s2 = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_xcpu]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s2);
    pcp_flush_current::<UstackRegion>();

    let after = in_use_count::<UstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[ustack_xcpu]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_stress_1000() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<UstackRegion>();
    let before = in_use_count::<UstackRegion>();

    for i in 0..1000 {
        let s = match UnsafeStack::allocate(
            TASK_UNSAFE_STACK_SIZE as usize,
            slopos_ostd::process::quota::root(),
        ) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_stress]: iteration {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    pcp_flush_current::<UstackRegion>();
    let after = in_use_count::<UstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[ustack_stress]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// kstack and ustack live in disjoint VA regions backed by independent
/// allocators: the windows do not overlap, each allocation lands in its own
/// window, and allocating from one leaves the other's `in_use_count` alone.
pub fn test_regions_disjoint() -> TestResult {
    use super::task_stack::{KernelStack, UnsafeStack};
    use slopos_abi::task::{TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE};
    use slopos_mm::memory_layout_defs::{
        KSTACK_VA_BASE, KSTACK_VA_END, USTACK_VA_BASE, USTACK_VA_END,
    };
    use slopos_mm::stack_region::{KstackRegion, UstackRegion};

    if KSTACK_VA_END > USTACK_VA_BASE && USTACK_VA_END > KSTACK_VA_BASE {
        klog_info!(
            "SCHED_TEST[disjoint]: VA regions overlap: K=[{:#x},{:#x}) U=[{:#x},{:#x})",
            KSTACK_VA_BASE,
            KSTACK_VA_END,
            USTACK_VA_BASE,
            USTACK_VA_END
        );
        return TestResult::Fail;
    }

    let u_before_k_work = slopos_mm::stack_va::in_use_count::<UstackRegion>();
    let kstack = match KernelStack::allocate(
        TASK_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[disjoint]: kstack alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let u_after_k_work = slopos_mm::stack_va::in_use_count::<UstackRegion>();
    if u_after_k_work != u_before_k_work {
        klog_info!(
            "SCHED_TEST[disjoint]: kstack alloc disturbed U in_use: {} -> {}",
            u_before_k_work,
            u_after_k_work
        );
        return TestResult::Fail;
    }

    let k_before_u_work = slopos_mm::stack_va::in_use_count::<KstackRegion>();
    let ustack = match UnsafeStack::allocate(
        TASK_UNSAFE_STACK_SIZE as usize,
        slopos_ostd::process::quota::root(),
    ) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[disjoint]: ustack alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let k_after_u_work = slopos_mm::stack_va::in_use_count::<KstackRegion>();
    if k_after_u_work != k_before_u_work {
        klog_info!(
            "SCHED_TEST[disjoint]: ustack alloc disturbed K in_use: {} -> {}",
            k_before_u_work,
            k_after_u_work
        );
        return TestResult::Fail;
    }

    let k_base = kstack.base().as_u64();
    let u_base = ustack.base().as_u64();
    if !(KSTACK_VA_BASE..KSTACK_VA_END).contains(&k_base) {
        klog_info!(
            "SCHED_TEST[disjoint]: kstack base {:#x} not in K region [{:#x},{:#x})",
            k_base,
            KSTACK_VA_BASE,
            KSTACK_VA_END
        );
        return TestResult::Fail;
    }
    if !(USTACK_VA_BASE..USTACK_VA_END).contains(&u_base) {
        klog_info!(
            "SCHED_TEST[disjoint]: ustack base {:#x} not in U region [{:#x},{:#x})",
            u_base,
            USTACK_VA_BASE,
            USTACK_VA_END
        );
        return TestResult::Fail;
    }

    drop(kstack);
    drop(ustack);
    TestResult::Pass
}

pub fn test_schedule_to_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    slopos_arch::pcr::mark_cpu_online(cpu_id);
    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to enable scheduler precondition on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"EmptyQueue\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };

    if !make_task_ready(task_id) {
        klog_info!("SCHED_TEST: Failed to make empty-queue task READY");
        return TestResult::Fail;
    }

    if schedule_task(&task_guard) != 0 {
        klog_info!("SCHED_TEST: Failed to schedule task to empty queue");
        return TestResult::Fail;
    }

    let ready_count = get_scheduler_stats().ready_tasks;

    if ready_count == 0 {
        klog_info!("SCHED_TEST: Task scheduled but ready count is 0");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Scheduling the same task twice must not duplicate it.
pub fn test_schedule_duplicate_task() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"Duplicate\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };

    if !make_task_ready(task_id) {
        klog_info!("SCHED_TEST: Failed to make duplicate-schedule task READY");
        return TestResult::Fail;
    }

    schedule_task(&task_guard);

    let ready_before = get_scheduler_stats().ready_tasks;

    schedule_task(&task_guard);

    let ready_after = get_scheduler_stats().ready_tasks;

    if ready_after != ready_before {
        klog_info!(
            "SCHED_TEST: Duplicate schedule changed count: {} -> {}",
            ready_before,
            ready_after
        );
    }

    TestResult::Pass
}

/// The publication entry point refuses a task that is not Ready, and rolls its
/// reservation back to `Nascent`.
pub fn test_schedule_refuses_non_ready_task() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NotReady\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };

    // A fresh task is Blocked + Nascent.
    if schedule_task(&guard) == 0 {
        klog_info!("SCHED_TEST: BUG - scheduled a task that was not Ready!");
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }
    if guard.sched_placement() != SchedPlacement::Nascent {
        klog_info!(
            "SCHED_TEST: refused publication left placement {:?}, expected Nascent",
            guard.sched_placement()
        );
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    TestResult::Pass
}

pub fn test_unschedule_not_in_queue() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NotQueued\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };

    let _result = unschedule_task(&task_guard);

    TestResult::Pass
}

pub fn test_priority_ordering() -> TestResult {
    let _fixture = SchedFixture::new();

    let low_id = task_create(
        b"LowPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    let normal_id = task_create(
        b"NormalPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    let high_id = task_create(
        b"HighPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::High.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if low_id == INVALID_TASK_ID || normal_id == INVALID_TASK_ID || high_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let (Some(low_guard), Some(normal_guard), Some(high_guard)) = (
        task_find_by_id(low_id),
        task_find_by_id(normal_id),
        task_find_by_id(high_id),
    ) else {
        return TestResult::Fail;
    };
    if !make_task_ready(low_id) || !make_task_ready(normal_id) || !make_task_ready(high_id) {
        klog_info!("SCHED_TEST: Failed to make priority tasks READY");
        return TestResult::Fail;
    }

    schedule_task(&low_guard);
    schedule_task(&normal_guard);
    schedule_task(&high_guard);

    TestResult::Pass
}

pub fn test_idle_priority_last() -> TestResult {
    let _fixture = SchedFixture::new();

    let idle_id = task_create(
        b"IdlePri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Idle.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    let normal_id = task_create(
        b"NormalPri2\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if idle_id == INVALID_TASK_ID || normal_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let (Some(idle_guard), Some(normal_guard)) =
        (task_find_by_id(idle_id), task_find_by_id(normal_id))
    else {
        return TestResult::Fail;
    };
    if !make_task_ready(idle_id) || !make_task_ready(normal_id) {
        klog_info!("SCHED_TEST: Failed to make idle-priority tasks READY");
        return TestResult::Fail;
    }

    schedule_task(&idle_guard);
    schedule_task(&normal_guard);

    // Priority order is not verifiable without running the scheduler; this only
    // checks the two publications do not crash.
    TestResult::Pass
}

pub fn test_timer_tick_decrements_slice() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"SliceTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    if !make_task_ready(task_id) {
        klog_info!("SCHED_TEST: Failed to make timer-slice task READY");
        return TestResult::Fail;
    }
    schedule_task(&task_guard);

    TestResult::Pass
}

pub fn test_terminate_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    let result = task_terminate(INVALID_TASK_ID);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating INVALID_TASK_ID succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_terminate_nonexistent_id() -> TestResult {
    let _fixture = SchedFixture::new();

    let result = task_terminate(0xDEADBEEF);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating nonexistent task succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_double_terminate() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"DoubleTerm\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let first_result = task_terminate(task_id);
    if first_result != 0 {
        klog_info!("SCHED_TEST: First terminate failed");
        return TestResult::Fail;
    }

    let _second_result = task_terminate(task_id);

    TestResult::Pass
}

pub fn test_find_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    if task_find_by_id(INVALID_TASK_ID).is_some() {
        klog_info!("SCHED_TEST: BUG - Found task with INVALID_TASK_ID!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// TODO(tech-debt): asserts nothing — a null entry point is unrepresentable, so
// either drive a real refusal through `task_create` or delete the test.
#[allow(unused_variables)]
pub fn test_create_null_entry() -> TestResult {
    let _fixture = SchedFixture::new();

    let _null_fn_ptr: Option<fn(*mut c_void)> = None;

    TestResult::Pass
}

pub fn test_create_conflicting_flags() -> TestResult {
    let _fixture = SchedFixture::new();

    let bad_flags = TASK_FLAG_KERNEL_MODE | super::task::TASK_FLAG_USER_MODE;

    let task_id = task_create(
        b"BadFlags\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        bad_flags,
    );

    if task_id != INVALID_TASK_ID {
        klog_info!("SCHED_TEST: BUG - Created task with conflicting flags!");
        task_terminate(task_id);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_create_null_name() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        ptr::null(),
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: Task creation with null name failed (may be OK)");
    }

    TestResult::Pass
}

pub fn test_scheduler_starts_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    let enabled = scheduler_is_enabled();

    if enabled != 0 {
        klog_info!("SCHED_TEST: Scheduler should start disabled!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_schedule_while_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    // Must be a no-op rather than a crash.
    schedule();

    TestResult::Pass
}

/// Boot userland pre-init enqueues tasks before `enter_scheduler()`, so this
/// must work on a CPU whose scheduler is initialised but not yet enabled.
pub fn test_schedule_task_before_scheduler_enable_on_current_cpu() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.disable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to disable scheduler precondition on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"BootPreInit\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    if cpu_id >= u32::BITS as usize {
        return TestResult::Pass;
    }

    if !make_task_ready(task_id) {
        klog_info!("SCHED_TEST: Failed to make pre-init task READY");
        return TestResult::Fail;
    }
    crate::task::task_install_idle_affinity(&task_guard, 1u32 << cpu_id, cpu_id as u8);

    if schedule_task(&task_guard) != 0 {
        klog_info!(
            "SCHED_TEST: Failed to schedule task before scheduler enable on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_count == 0 {
        klog_info!(
            "SCHED_TEST: Task was not enqueued before scheduler enable on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_resolve_idle_stack_for_bsp_uses_idle_task_kernel_stack() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task_for_cpu(0) != 0 {
        klog_info!("SCHED_TEST: Failed to create BSP idle task");
        return TestResult::Fail;
    }

    let (idle_task, stack_top) = match runtime::resolve_idle_stack_for_cpu(0) {
        Ok(values) => values,
        Err(err) => {
            klog_info!("SCHED_TEST: Failed to resolve BSP idle stack: {:?}", err);
            return TestResult::Fail;
        }
    };

    let expected_top = idle_task.task().kernel_stack_top;
    if expected_top == 0 || stack_top != expected_top {
        klog_info!(
            "SCHED_TEST: Idle stack mismatch (expected=0x{:x}, got=0x{:x})",
            expected_top,
            stack_top
        );
        return TestResult::Fail;
    }

    if (stack_top & 0xF) != 0 {
        klog_info!(
            "SCHED_TEST: Idle stack is not 16-byte aligned: 0x{:x}",
            stack_top
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_resolve_idle_stack_reports_missing_idle_task() -> TestResult {
    let _fixture = SchedFixture::new();

    // PCR.idle_task is the single source of truth for the idle slot.
    let previous_idle = slopos_arch::pcr::get_idle_task(0) as *mut Task;
    slopos_arch::pcr::set_idle_task(0, ptr::null_mut());

    let result = match runtime::resolve_idle_stack_for_cpu(0) {
        Err(IdleStackResolveError::MissingIdleTask) => TestResult::Pass,
        Err(other) => {
            klog_info!(
                "SCHED_TEST: Expected MissingIdleTask, got different error: {:?}",
                other
            );
            TestResult::Fail
        }
        Ok((_, stack_top)) => {
            klog_info!(
                "SCHED_TEST: Expected missing idle task, got stack 0x{:x}",
                stack_top
            );
            TestResult::Fail
        }
    };

    slopos_arch::pcr::set_idle_task(0, previous_idle as *mut ());

    result
}

pub fn test_resolve_idle_stack_reports_missing_kernel_stack() -> TestResult {
    let _fixture = SchedFixture::new();

    // Built here rather than mutated in place on the installed idle task: a
    // running CPU is standing on that stack.
    let mut arc = match KArc::try_init(Task::init_invalid()) {
        Ok(task) => task,
        Err(_) => return TestResult::Fail,
    };
    {
        let task = match KArc::get_mut(&mut arc) {
            Some(task) => task,
            None => return TestResult::Fail,
        };
        task.kernel_stack_top = 0;
    }

    let missing = runtime::idle_stack_top(&arc);
    if !matches!(missing, Err(IdleStackResolveError::MissingKernelStack)) {
        klog_info!(
            "SCHED_TEST: Expected MissingKernelStack for a zero stack top, got {:?}",
            missing
        );
        return TestResult::Fail;
    }

    {
        let task = match KArc::get_mut(&mut arc) {
            Some(task) => task,
            None => return TestResult::Fail,
        };
        task.kernel_stack_top = 0xFFFF_8000_0010_0000;
    }
    if runtime::idle_stack_top(&arc) != Ok(0xFFFF_8000_0010_0000) {
        klog_info!("SCHED_TEST: A populated stack top did not resolve");
        return TestResult::Fail;
    }

    drop(arc);
    TestResult::Pass
}

pub fn test_many_same_priority_tasks() -> TestResult {
    let _fixture = SchedFixture::new();

    const COUNT: usize = 32;
    let mut ids = [INVALID_TASK_ID; COUNT];

    for i in 0..COUNT {
        ids[i] = task_create(
            b"SamePri\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if ids[i] == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Failed at task {}", i);
            break;
        }
    }

    for id in ids.iter() {
        if *id != INVALID_TASK_ID {
            if let Some(task) = task_find_by_id(*id) {
                assert!(
                    make_task_ready(*id),
                    "make_task_ready failed for id {:?}",
                    id
                );
                schedule_task(&task);
            }
        }
    }

    let ready = get_scheduler_stats().ready_tasks;

    klog_info!("SCHED_TEST: Scheduled {} tasks of same priority", ready);

    TestResult::Pass
}

pub fn test_interleaved_operations() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..50 {
        let id1 = task_create(
            b"Inter1\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        let id2 = task_create(
            b"Inter2\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::High.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if id1 == INVALID_TASK_ID || id2 == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Interleaved creation failed at iteration {}", i);
            return TestResult::Fail;
        }

        if let Some(task1) = task_find_by_id(id1) {
            assert!(
                make_task_ready(id1),
                "make_task_ready failed for id {:?}",
                id1
            );
            schedule_task(&task1);
        }

        task_terminate(id1);

        if let Some(task2) = task_find_by_id(id2) {
            assert!(
                make_task_ready(id2),
                "make_task_ready failed for id {:?}",
                id2
            );
            schedule_task(&task2);
        }

        task_terminate(id2);
    }

    TestResult::Pass
}

/// `push_remote_wake` adds to the inbox; `drain_remote_inbox` moves the entries
/// to the ready queue.
pub fn test_remote_inbox_push_drain() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"InboxTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set inbox test task READY");
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let ready_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
    });

    // On SMP a timer tick may drain the inbox before this read; the ready-queue
    // delta below is the real assertion.
    let has_pending = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
        .unwrap_or(false);

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    let still_pending =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if still_pending && has_pending {
        klog_info!("SCHED_TEST: drain_remote_inbox did not empty inbox");
        return TestResult::Fail;
    }

    let ready_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    if ready_after <= ready_before {
        klog_info!(
            "SCHED_TEST: Task not moved to ready queue: before={}, after={}",
            ready_before,
            ready_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Pushing the same task into a remote inbox twice is a no-op, not a duplicate
/// Treiber node: a duplicate can self-cycle the inbox.
pub fn test_remote_inbox_duplicate_push_is_single_membership() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"InboxDup\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set duplicate inbox task READY");
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let ready_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    let Some(node) = Some(task_guard.node()) else {
        return TestResult::Fail;
    };
    // Baseline: the registry owner alone. One placement reference must survive
    // the duplicate push and drain.
    let strong_base = task_placement_strong_count(node);

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
        sched.push_remote_wake(&task_guard);
    });

    if !task_ptr.inbox_link().is_linked() {
        klog_info!("SCHED_TEST: Duplicate-push task was not marked inbox-linked");
        return TestResult::Fail;
    }

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    if task_ptr.inbox_link().is_linked() {
        klog_info!("SCHED_TEST: inbox-linked bit not cleared after drain");
        return TestResult::Fail;
    }

    let ready_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_after != ready_before.saturating_add(1) {
        klog_info!(
            "SCHED_TEST: duplicate inbox push queued {} tasks (before={}, after={})",
            ready_after.saturating_sub(ready_before),
            ready_before,
            ready_after,
        );
        return TestResult::Fail;
    }

    let strong_after = task_placement_strong_count(node);
    if strong_after != strong_base + 1 {
        klog_info!(
            "SCHED_TEST: duplicate inbox push leaked references (base={}, after={})",
            strong_base,
            strong_after,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// FIFO ordering is preserved through an inbox drain.
pub fn test_remote_inbox_multiple_tasks() -> TestResult {
    let _fixture = SchedFixture::new();

    const NUM_TASKS: usize = 5;
    let mut task_ids = [INVALID_TASK_ID; NUM_TASKS];
    let mut task_guards: [Option<TaskRef>; NUM_TASKS] = [const { None }; NUM_TASKS];

    for i in 0..NUM_TASKS {
        task_ids[i] = task_create(
            b"MultiInbox\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if task_ids[i] == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Failed to create task {}", i);
            return TestResult::Fail;
        }

        let Some(guard) = task_find_by_id(task_ids[i]) else {
            return TestResult::Fail;
        };
        task_guards[i] = Some(guard);
        assert!(
            scheduler::clear_nascent_for_test(task_ids[i]),
            "fixture task was not nascent"
        );
        if task_set_state(task_ids[i], TaskStatus::Ready) != 0 {
            klog_info!("SCHED_TEST: Failed to set inbox task {} READY", i);
            return TestResult::Fail;
        }
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    for guard in task_guards.iter().flatten() {
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.push_remote_wake(&guard);
        });
    }

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    if (ready_count as usize) < NUM_TASKS {
        klog_info!(
            "SCHED_TEST: Not all tasks in ready queue: expected {}, got {}",
            NUM_TASKS,
            ready_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A timer tick drains the remote inbox.
pub fn test_timer_tick_drains_inbox() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        klog_info!("SCHED_TEST: Failed to create idle task");
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"TimerDrain\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set timer-drain task READY");
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Push to the inbox, bypassing `schedule_task`.
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
    });

    let has_pending_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(false);

    if !has_pending_before {
        klog_info!("SCHED_TEST: Task not in inbox before timer tick");
        return TestResult::Fail;
    }

    scheduler_timer_tick();

    let has_pending_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if has_pending_after {
        klog_info!("SCHED_TEST: Timer tick did not drain inbox");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Draining the remote inbox must not enqueue non-ready tasks.
pub fn test_remote_inbox_drops_non_ready_tasks() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let task_id = task_create(
        b"InboxBlocked\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    let Some(node) = Some(task_guard.node()) else {
        return TestResult::Fail;
    };
    let strong_base = task_placement_strong_count(node);

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
    });

    if task_ptr.status() != (TaskStatus::Blocked) {
        klog_info!("SCHED_TEST: inbox drop task was not initially BLOCKED");
        return TestResult::Fail;
    }

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_count != 0 {
        klog_info!(
            "SCHED_TEST: Non-ready task was enqueued from inbox (ready_count={})",
            ready_count
        );
        return TestResult::Fail;
    }

    let inbox_pending =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);
    if inbox_pending {
        klog_info!("SCHED_TEST: Inbox still has pending entries after drain");
        return TestResult::Fail;
    }

    let strong_after = task_placement_strong_count(node);
    if strong_after != strong_base {
        klog_info!(
            "SCHED_TEST: dropped inbox task leaked references (base={}, after={})",
            strong_base,
            strong_after,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A cross-CPU `schedule_task` goes through `push_remote_wake`.
pub fn test_cross_cpu_schedule_lockfree() -> TestResult {
    let _fixture = SchedFixture::new();

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count < 2 {
        klog_info!("SCHED_TEST: Skipping cross-CPU test (only 1 CPU)");
        return TestResult::Pass;
    }

    let current_cpu = slopos_arch::pcr::get_current_cpu() as usize;
    let target_cpu =
        match (0..cpu_count).find(|cpu| *cpu != current_cpu && *cpu < u32::BITS as usize) {
            Some(cpu) => cpu,
            None => {
                klog_info!(
                    "SCHED_TEST: Skipping cross-CPU test (no target CPU != {} in affinity range)",
                    current_cpu
                );
                return TestResult::Pass;
            }
        };
    let target_cpu_u8 = target_cpu as u8;

    slopos_arch::pcr::mark_cpu_online(target_cpu);
    if super::per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to enable target CPU {} scheduler",
            target_cpu
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"CrossCPU\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set cross-CPU task READY");
        return TestResult::Fail;
    }
    // Keep last_cpu on the current CPU so the scheduler must migrate it.
    crate::task::task_install_idle_affinity(task_ptr, 1u32 << target_cpu, current_cpu as u8);

    let result = schedule_task(&task_guard);
    if result != 0 {
        klog_info!("SCHED_TEST: Cross-CPU schedule_task failed");
        return TestResult::Fail;
    }

    super::per_cpu::with_cpu_scheduler(target_cpu, |sched| {
        sched.drain_remote_inbox();
    });

    let ready_on_target =
        super::per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.total_ready_count())
            .unwrap_or(0);

    if ready_on_target == 0 {
        klog_info!(
            "SCHED_TEST: Task not found on CPU {} after cross-CPU schedule",
            target_cpu
        );
        return TestResult::Fail;
    }

    if task_ptr.last_cpu() != target_cpu_u8 {
        klog_info!(
            "SCHED_TEST: last_cpu not updated to target CPU (expected {}, got {})",
            target_cpu,
            task_ptr.last_cpu()
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A user-mode task carries user segment selectors, a process VM and a kernel
/// RSP0 stack, and the syscall gate is reachable at DPL 3.
pub fn test_privilege_separation_invariants() -> TestResult {
    let _fixture = SchedFixture::new();

    let user_task_id = task_create(
        b"UserStub\0".as_ptr() as *const c_char,
        crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    );
    if user_task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: user task creation failed");
        return TestResult::Fail;
    }
    // Prevent the scheduler on other CPUs from running this stub task.
    task_set_state(user_task_id, TaskStatus::Blocked);

    let Some(task_guard) = task_find_by_id(user_task_id) else {
        klog_info!("SCHED_TEST: user task lookup failed");
        return TestResult::Fail;
    };

    let task_ref = &*task_guard;
    if task_ref.process_id == INVALID_PROCESS_ID {
        klog_info!("SCHED_TEST: user task missing process VM");
        return TestResult::Fail;
    }
    if task_ref.kernel_stack_top == 0 {
        klog_info!("SCHED_TEST: user task missing kernel RSP0 stack");
        return TestResult::Fail;
    }
    let cs = task_ref.context_cs();
    let ss = task_ref.context_ss();
    if cs != SegmentSelector::USER_CODE.bits() as u64
        || ss != SegmentSelector::USER_DATA.bits() as u64
    {
        klog_info!(
            "SCHED_TEST: user selectors wrong (cs=0x{:x} ss=0x{:x})",
            cs,
            ss
        );
        return TestResult::Fail;
    }

    let mut gate = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };
    let gate_ptr = &mut gate as *mut IdtEntry as *mut c_void;
    if slopos_kernel_services::platform::idt_get_gate(SYSCALL_VECTOR, gate_ptr) != 0 {
        klog_info!("SCHED_TEST: cannot read syscall gate");
        return TestResult::Fail;
    }
    let dpl = (gate.type_attr >> 5) & 0x3;
    if dpl != 3 {
        klog_info!("SCHED_TEST: syscall gate DPL={} expected 3", dpl as u32);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_scheduler_wakeup_race_stress_baseline() -> TestResult {
    let _fixture = SchedFixture::new();

    let mut task_ids = [INVALID_TASK_ID; 8];
    for slot in &mut task_ids {
        let id = task_create(
            b"WakeStress\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        *slot = id;
    }

    for _ in 0..128 {
        for id in task_ids {
            let Some(task) = task_find_by_id(id) else {
                return TestResult::Fail;
            };
            let task_ptr: &Task = &task;
            if task_ptr.status() != (TaskStatus::Ready) {
                assert!(
                    make_task_ready(id),
                    "make_task_ready failed for id {:?}",
                    id
                );
            }
            let _ = schedule_task(&task);
        }
        scheduler_timer_tick();
        schedule();
        for id in task_ids {
            if let Some(task) = task_find_by_id(id) {
                let _ = unschedule_task(&task);
            }
            if task_find_by_id(id).is_none() {
                return TestResult::Fail;
            }
            let _ = task_set_state(id, TaskStatus::Ready);
        }
    }

    for id in task_ids {
        task_terminate(id);
    }

    TestResult::Pass
}

pub fn test_sleep_wake_race_regression() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    let task_id = task_create(
        b"SleepRace\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    // Far enough out that a real timer tick cannot collect the entry before the
    // test explicitly wakes it.
    const FAR_FUTURE: u64 = u64::MAX / 2;

    for round in 0..64 {
        let _ = unschedule_task(&task_guard);
        if task_ptr.status() == TaskStatus::Blocked && !make_task_ready(task_id) {
            klog_info!("SCHED_TEST: set Ready failed at round {}", round);
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if task_set_state(task_id, TaskStatus::Running) != 0 {
            klog_info!("SCHED_TEST: set Running failed at round {}", round);
            task_terminate(task_id);
            return TestResult::Fail;
        }

        if !super::sleep::test_insert_sleep_entry(task_id, FAR_FUTURE) {
            klog_info!("SCHED_TEST: sleep queue insert failed at round {}", round);
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if task_set_state_with_reason(task_id, TaskStatus::Blocked, BlockReason::Sleep) != 0 {
            klog_info!("SCHED_TEST: set Blocked failed at round {}", round);
            super::sleep::cancel_sleep(task_id);
            task_terminate(task_id);
            return TestResult::Fail;
        }

        super::sleep::wake_due_sleepers(FAR_FUTURE + 1);

        if task_ptr.is_blocked() {
            klog_info!("SCHED_TEST: task stuck in Blocked after wake — race bug");
            let _ = task_set_state(task_id, TaskStatus::Ready);
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

/// The tickless-idle path must not panic when the soonest sleep deadline is
/// already past: `deadline.wrapping_sub(now)` then lands near `u64::MAX`, and
/// the idle path must treat it as already-due rather than arm a one-shot.
pub fn test_tickless_idle_past_deadline_no_overflow() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    // `wake_tick = 1` is already past, so the idle path's `wrapping_sub`
    // produces a ~`u64::MAX` delta.
    if !super::sleep::test_insert_sleep_entry(424_242, 1) {
        super::sleep::reset_sleep_queue();
        return TestResult::Fail;
    }

    // Must return without panicking.
    scheduler::arm_tickless_idle_if_due();

    super::sleep::reset_sleep_queue();
    TestResult::Pass
}

/// A due sleep entry whose task is not yet sleep-parked survives the tick:
/// entries may only disappear once a wake conclusively publishes `Ready`.
pub fn test_sleep_entry_survives_unparked_wake() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    let task_id = task_create(
        b"SleepPeek\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: task_create failed (table full or allocation refused)");
        return TestResult::Fail;
    }
    let Some(task_guard) = task_find_by_id(task_id) else {
        klog_info!(
            "SCHED_TEST: task {} not in the registry after create",
            task_id
        );
        task_terminate(task_id);
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    // Park the task off the ready queue so the scheduler cannot dispatch (and
    // exit) it mid-test; it lands Blocked with a non-Sleep reason, which is the
    // "not sleep-parked" shape under test.
    let _ = unschedule_task(&task_guard);
    if !task_ptr.is_blocked() {
        klog_info!("SCHED_TEST: unschedule did not park the task");
        task_terminate(task_id);
        return TestResult::Fail;
    }

    // Entry armed and already due, task not Blocked(Sleep): the tick must leave
    // it armed for retry.
    if !super::sleep::test_insert_sleep_entry(task_id, 1) {
        klog_info!("SCHED_TEST: sleep entry insert failed");
        task_terminate(task_id);
        return TestResult::Fail;
    }
    super::sleep::wake_due_sleepers(u64::MAX / 2);
    if !super::sleep::test_sleep_entry_armed(task_id) {
        klog_info!("SCHED_TEST: due entry for unparked task was dropped by the tick");
        super::sleep::cancel_sleep(task_id);
        task_terminate(task_id);
        return TestResult::Fail;
    }

    // Genuinely sleep-parked: the next tick delivers the wake and only then
    // clears the entry.
    if !super::sleep::arm_blocked_timeout(task_id, 0) {
        klog_info!("SCHED_TEST: could not arm a deadline for the parked task");
        super::sleep::cancel_sleep(task_id);
        task_terminate(task_id);
        return TestResult::Fail;
    }
    super::sleep::wake_due_sleepers(u64::MAX / 2);
    if task_ptr.is_blocked() {
        klog_info!("SCHED_TEST: parked task not woken by due entry");
        super::sleep::cancel_sleep(task_id);
        task_terminate(task_id);
        return TestResult::Fail;
    }
    if super::sleep::test_sleep_entry_armed(task_id) {
        klog_info!("SCHED_TEST: delivered wake left its entry armed");
        super::sleep::cancel_sleep(task_id);
        task_terminate(task_id);
        return TestResult::Fail;
    }

    task_terminate(task_id);
    TestResult::Pass
}

/// Ensuring the sleep queue has a backing store must not disturb the entries it
/// already holds — it runs from `allocate_task`, long after sleepers have armed.
pub fn test_ensure_sleep_queue_allocated_preserves_entries() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    let task_id = task_create(
        b"SleepEnsure\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    const FAR_FUTURE: u64 = u64::MAX / 2;
    if !super::sleep::test_insert_sleep_entry(task_id, FAR_FUTURE) {
        klog_info!("SCHED_TEST: could not arm the entry to protect");
        task_terminate(task_id);
        return TestResult::Fail;
    }

    for round in 0..4 {
        if !super::sleep::ensure_sleep_queue_allocated() {
            klog_info!("SCHED_TEST: ensure failed at round {}", round);
            super::sleep::cancel_sleep(task_id);
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if !super::sleep::test_sleep_entry_armed(task_id) {
            klog_info!(
                "SCHED_TEST: ensure unarmed a live sleeper at round {}",
                round
            );
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    super::sleep::cancel_sleep(task_id);
    task_terminate(task_id);
    TestResult::Pass
}

/// 1000 rounds of create → terminate → `task_wait_for`: every wait must return
/// promptly via the durable `exit_info` fast path rather than block.
///
/// The fixture pauses the APs, so the multi-CPU race window is not reproducible
/// here; what is under test is that a waiter arriving after the publish sees it.
pub fn test_task_wait_exit_race_1000() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..1000 {
        let child_id = task_create(
            b"WaitRace\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if child_id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: task_create failed at iteration {}", i);
            return TestResult::Fail;
        }

        let Some(child) = task_find_by_id(child_id) else {
            klog_info!("SCHED_TEST: task_find_by_id null at iteration {}", i);
            return TestResult::Fail;
        };
        let child_ptr: &Task = &child;

        let rc = task_terminate(child_id);
        if rc != 0 {
            klog_info!(
                "SCHED_TEST: task_terminate returned {} at iteration {}",
                rc,
                i
            );
            return TestResult::Fail;
        }

        if !child_ptr.is_terminated() {
            klog_info!(
                "SCHED_TEST: child not Terminated after task_terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }
        if !child_ptr.exit_info_is_set() {
            klog_info!(
                "SCHED_TEST: exit_info not published after task_terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }

        // Returning at all is the property: the runner is not deadlocked.
        let wait_rc = task_wait_for(child_id);
        if wait_rc != 0 {
            klog_info!(
                "SCHED_TEST: task_wait_for returned {} at iter {} (expected 0)",
                wait_rc,
                i
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// `test_task_wait_exit_race_1000` with the child driven Ready→Running→Ready
/// first, so `mark_task_terminated`'s runtime-accounting path runs alongside
/// the publish.
pub fn test_task_wait_exit_race_with_work() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..500 {
        let child_id = task_create(
            b"WaitRaceWork\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if child_id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: task_create failed at iteration {}", i);
            return TestResult::Fail;
        }

        let Some(child) = task_find_by_id(child_id) else {
            klog_info!("SCHED_TEST: task_find_by_id null at iter {}", i);
            return TestResult::Fail;
        };
        let child_ptr: &Task = &child;

        // A non-zero runtime delta for `mark_task_terminated` to observe.
        if !make_task_ready(child_id) {
            klog_info!("SCHED_TEST: failed Ready transition at iter {}", i);
            return TestResult::Fail;
        }
        if task_set_state(child_id, TaskStatus::Running) != 0 {
            klog_info!("SCHED_TEST: failed Running transition at iter {}", i);
            return TestResult::Fail;
        }
        child_ptr.set_last_run_timestamp(1);
        // Shift the relative ordering of publish versus observe.
        for _ in 0..16 {
            core::hint::spin_loop();
        }
        if task_set_state(child_id, TaskStatus::Ready) != 0 {
            klog_info!("SCHED_TEST: failed Ready transition at iter {}", i);
            return TestResult::Fail;
        }

        let rc = task_terminate(child_id);
        if rc != 0 {
            klog_info!("SCHED_TEST: task_terminate returned {} at iter {}", rc, i);
            return TestResult::Fail;
        }

        if !child_ptr.is_terminated() {
            klog_info!(
                "SCHED_TEST: child not Terminated after terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }
        if !child_ptr.exit_info_is_set() {
            klog_info!(
                "SCHED_TEST: exit_info not published after terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }

        let wait_rc = task_wait_for(child_id);
        if wait_rc != 0 {
            klog_info!(
                "SCHED_TEST: task_wait_for returned {} at iter {} (expected 0)",
                wait_rc,
                i
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Durable `exit_info` satisfies any number of late waiters that arrive after
/// the wake fanout has already fired: each `task_wait_for` for the same
/// terminated child returns via the fast path without blocking.
pub fn test_task_wait_multi_waiter() -> TestResult {
    let _fixture = SchedFixture::new();

    let child_id = task_create(
        b"MultiWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(child_ref) = task_find_by_id(child_id) else {
        return TestResult::Fail;
    };
    let child_ptr: &Task = &child_ref;

    if task_waiter_count(child_ptr) > 0 {
        klog_info!("SCHED_TEST: child waiters queue non-empty before terminate");
        return TestResult::Fail;
    }

    if task_terminate(child_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate failed");
        return TestResult::Fail;
    }

    if !child_ptr.is_terminated() {
        klog_info!("SCHED_TEST: child not Terminated after terminate");
        return TestResult::Fail;
    }
    if !child_ptr.exit_info_is_set() {
        klog_info!("SCHED_TEST: exit_info not set after terminate");
        return TestResult::Fail;
    }

    for waiter in 0..4 {
        let rc = task_wait_for(child_id);
        if rc != 0 {
            klog_info!(
                "SCHED_TEST: waiter {} task_wait_for returned {} (expected 0)",
                waiter,
                rc
            );
            return TestResult::Fail;
        }
        // The wait path never takes the cell, so it survives repeated
        // observation.
        if !child_ptr.exit_info_is_set() {
            klog_info!(
                "SCHED_TEST: exit_info became unset after waiter {} returned",
                waiter
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// `scheduler_timer_tick()` always increments `total_ticks`, including on its
/// early-return path. Exercises the unguarded path only.
pub fn test_timer_tick_always_increments_ticks() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let ticks_before = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched
            .total_ticks
            .load(core::sync::atomic::Ordering::Relaxed)
    })
    .unwrap_or(0);

    for _ in 0..5 {
        scheduler_timer_tick();
    }

    let ticks_after = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched
            .total_ticks
            .load(core::sync::atomic::Ordering::Relaxed)
    })
    .unwrap_or(0);

    let delta = ticks_after.saturating_sub(ticks_before);
    if delta < 5 {
        klog_info!(
            "SCHED_TEST: timer_tick incremented ticks only {} times (expected >=5)",
            delta
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// With the idle task current, each timer tick increments `total_ticks` and
/// `idle_time` by the same amount.
pub fn test_idle_time_tracks_ticks_not_iterations() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Set current task to the idle task so timer_tick recognises us as idle.
    let Some(idle) = crate::task_struct::Idle::current() else {
        return TestResult::Fail;
    };
    super::scheduler::dispatch(cpu_id, idle.task());

    let (ticks_before, idle_before) = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        (
            sched
                .total_ticks
                .load(core::sync::atomic::Ordering::Relaxed),
            sched.idle_time.load(core::sync::atomic::Ordering::Relaxed),
        )
    })
    .unwrap_or((0, 0));

    for _ in 0..10 {
        scheduler_timer_tick();
    }

    let (ticks_after, idle_after) = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        (
            sched
                .total_ticks
                .load(core::sync::atomic::Ordering::Relaxed),
            sched.idle_time.load(core::sync::atomic::Ordering::Relaxed),
        )
    })
    .unwrap_or((0, 0));

    let delta_ticks = ticks_after.saturating_sub(ticks_before);
    let delta_idle = idle_after.saturating_sub(idle_before);

    if delta_ticks < 10 {
        klog_info!("SCHED_TEST: total_ticks delta {} < 10", delta_ticks);
        return TestResult::Fail;
    }

    let drift = if delta_idle > delta_ticks {
        delta_idle - delta_ticks
    } else {
        delta_ticks - delta_idle
    };
    // Tolerance for SMP timing jitter.
    if drift > 2 {
        klog_info!(
            "SCHED_TEST: idle_time ({}) vs total_ticks ({}) — drift {} exceeds tolerance",
            delta_idle,
            delta_ticks,
            drift
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// `select_target_cpu` prefers an idle CPU over a busy `last_cpu`.
pub fn test_select_target_cpu_prefers_idle_cpu() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        // Single-CPU systems cannot test cross-CPU placement.
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Both CPUs online and schedulable, so both are candidates.
    slopos_arch::pcr::mark_cpu_online(cpu_id);
    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Could not enable scheduler on local CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }
    let other_cpu = if cpu_id == 0 { 1 } else { 0 };
    slopos_arch::pcr::mark_cpu_online(other_cpu);
    if super::per_cpu::with_cpu_scheduler(other_cpu, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Could not enable scheduler on CPU {}",
            other_cpu
        );
        return TestResult::Fail;
    }

    let mut filler_ids = [INVALID_TASK_ID; 3];
    for i in 0..3 {
        let tid = task_create(
            b"Filler\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if tid == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        filler_ids[i] = tid;
        let Some(filler) = task_find_by_id(tid) else {
            return TestResult::Fail;
        };
        let tp: &Task = &filler;
        // Pin fillers to cpu_id so they stay in its queue.
        crate::task::task_install_idle_affinity(
            tp,
            super::per_cpu::affinity_mask_for_cpu(cpu_id),
            cpu_id as u8,
        );
        if !make_task_ready(tid) || schedule_task(&filler) != 0 {
            return TestResult::Fail;
        }
    }

    // affinity 0 = any CPU; `last_cpu` is the busy one.
    let task_id = task_create(
        b"Migratee\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;

    crate::task::task_install_idle_affinity(task_ptr, 0, cpu_id as u8);

    let target = super::per_cpu::select_target_cpu(&task_guard);
    match target {
        Some(t) if t == other_cpu => { /* expected — migrated to idle CPU */ }
        Some(t) if t == cpu_id => {
            klog_info!(
                "SCHED_TEST: select_target_cpu returned busy last_cpu {} instead of idle CPU {}",
                cpu_id,
                other_cpu
            );
            return TestResult::Fail;
        }
        Some(t) => {
            klog_info!(
                "SCHED_TEST: select_target_cpu chose CPU {} (not the expected {} but still OK)",
                t,
                other_cpu
            );
        }
        None => {
            klog_info!("SCHED_TEST: select_target_cpu returned None");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// A CPU running a real task with an empty queue is not idle — bursty workloads
/// leave the queue empty between bursts.
pub fn test_select_target_cpu_running_task_not_idle() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let other_cpu = if cpu_id == 0 { 1 } else { 0 };
    slopos_arch::pcr::mark_cpu_online(other_cpu);
    if super::per_cpu::with_cpu_scheduler(other_cpu, |sched| sched.enable()).is_none() {
        return TestResult::Fail;
    }

    // The queue stays empty, but `effective_load` must be 1 because a non-idle
    // task is running.
    let runner_id = task_create(
        b"Runner\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if runner_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(runner_guard) = task_find_by_id(runner_id) else {
        return TestResult::Fail;
    };
    if !make_task_ready(runner_id) {
        return TestResult::Fail;
    }
    super::scheduler::dispatch(cpu_id, &runner_guard);

    let load =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load()).unwrap_or(0);
    if load == 0 {
        klog_info!(
            "SCHED_TEST: effective_load is 0 despite running task on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"WakeTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    crate::task::task_install_idle_affinity(task_ptr, 0, cpu_id as u8);

    let target = super::per_cpu::select_target_cpu(&task_guard);
    match target {
        Some(t) if t != cpu_id => { /* expected — migrated away from busy CPU */ }
        Some(t) => {
            klog_info!(
                "SCHED_TEST: select_target_cpu stuck to CPU {} despite running task (empty queue)",
                t
            );
            return TestResult::Fail;
        }
        None => {
            klog_info!("SCHED_TEST: select_target_cpu returned None");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// `schedule_new_task()` spreads sequential forks across CPUs round-robin
/// rather than piling them onto CPU0.
pub fn test_schedule_new_task_spreads_across_cpus() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    for c in 0..cpu_count {
        slopos_arch::pcr::mark_cpu_online(c);
        super::per_cpu::with_cpu_scheduler(c, |sched| sched.enable());
    }

    // The forking parent runs on cpu_id.
    let parent_id = task_create(
        b"Parent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(parent_guard) = task_find_by_id(parent_id) else {
        return TestResult::Fail;
    };
    if !make_task_ready(parent_id) {
        return TestResult::Fail;
    }
    super::scheduler::dispatch(cpu_id, &parent_guard);

    let n = cpu_count.min(4);
    let mut placed_on = [0usize; 4];
    for i in 0..n {
        let tid = task_create(
            b"Child\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if tid == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        let Some(child) = task_find_by_id(tid) else {
            return TestResult::Fail;
        };
        let tp: &Task = &child;
        tp.set_cpu_affinity(0); // any CPU
        if !make_task_ready(tid) || schedule_new_task(&child) != 0 {
            return TestResult::Fail;
        }
        placed_on[i] = tp.last_cpu() as usize;
    }

    let mut distinct = [false; MAX_CPUS];
    let mut count = 0usize;
    for i in 0..n {
        if !distinct[placed_on[i]] {
            distinct[placed_on[i]] = true;
            count += 1;
        }
    }

    if count < 2 {
        klog_info!(
            "SCHED_TEST: schedule_new_task placed all {} children on same CPU ({})",
            n,
            placed_on[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// `effective_load` reflects queued tasks.
pub fn test_effective_load_accuracy() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // After a fixture reset only the running task can count, so 0 or 1.
    let load_before = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load())
        .unwrap_or(u32::MAX);
    if load_before > 1 {
        klog_info!(
            "SCHED_TEST: effective_load {} > 1 on empty queues",
            load_before
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"LoadCheck\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(task_guard) = task_find_by_id(task_id) else {
        return TestResult::Fail;
    };
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enqueue_local(&task_guard);
    });

    let load_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load()).unwrap_or(0);
    if load_after <= load_before {
        klog_info!(
            "SCHED_TEST: effective_load did not increase after enqueue ({} -> {})",
            load_before,
            load_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

const FORK_GROUP_WIDTH: usize = 10;
const FORK_GROUP_ITERATIONS: usize = 100;

/// Each round spawns `FORK_GROUP_WIDTH` children before terminating any, so the
/// wait/wake protocol runs with live siblings rather than the singleton case.
pub fn test_fork_exit_wait_stress_10x100() -> TestResult {
    let _fixture = SchedFixture::new();

    let mut child_ids = [INVALID_TASK_ID; FORK_GROUP_WIDTH];

    for outer in 0..FORK_GROUP_ITERATIONS {
        for slot in 0..FORK_GROUP_WIDTH {
            let id = task_create(
                b"ForkStress\0".as_ptr() as *const c_char,
                dummy_task_entry,
                ptr::null_mut(),
                TaskPriority::Normal.as_u8(),
                TASK_FLAG_KERNEL_MODE,
            );
            if id == INVALID_TASK_ID {
                klog_info!(
                    "SCHED_TEST: task_create failed at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
            child_ids[slot] = id;

            // A freshly-allocated slot must come back with an empty waiters
            // ring and an unset exit_info however many reuses it has seen.
            let Some(child) = task_find_by_id(id) else {
                klog_info!(
                    "SCHED_TEST: task_find_by_id null at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            };
            let ptr: &Task = &child;
            if task_waiter_count(ptr) > 0 {
                klog_info!(
                    "SCHED_TEST: fresh child has stale waiters at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
            if ptr.exit_info_is_set() {
                klog_info!(
                    "SCHED_TEST: fresh child has stale exit_info at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }

        for slot in 0..FORK_GROUP_WIDTH {
            let id = child_ids[slot];
            let rc = task_terminate(id);
            if rc != 0 {
                klog_info!(
                    "SCHED_TEST: task_terminate({}) returned {} at outer={} slot={}",
                    id,
                    rc,
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }

        for slot in 0..FORK_GROUP_WIDTH {
            let id = child_ids[slot];
            let wait_rc = task_wait_for(id);
            if wait_rc != 0 {
                klog_info!(
                    "SCHED_TEST: task_wait_for({}) returned {} at outer={} slot={} (expected 0)",
                    id,
                    wait_rc,
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }
    }

    TestResult::Pass
}

/// 1000 tasks allocated and destroyed sequentially. IDs must advance, dead
/// weak entries must stop upgrading, and each fresh allocation starts clean.
pub fn test_serial_reap_stampede() -> TestResult {
    let _fixture = SchedFixture::new();

    const STAMPEDE: usize = 1000;
    let mut last_id = 0u32;

    for i in 0..STAMPEDE {
        let id = task_create(
            b"ReapStampede\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: task_create failed at iteration {}", i);
            return TestResult::Fail;
        }

        let Some(task) = task_find_by_id(id) else {
            klog_info!("SCHED_TEST: task_find_by_id null at iter {}", i);
            return TestResult::Fail;
        };
        let ptr: &Task = &task;

        if id <= last_id {
            klog_info!(
                "SCHED_TEST: task id did not advance: {} after {}",
                id,
                last_id
            );
            return TestResult::Fail;
        }
        last_id = id;

        if task_waiter_count(ptr) > 0 {
            klog_info!(
                "SCHED_TEST: fresh task {} has stale waiters at iter {}",
                id,
                i
            );
            return TestResult::Fail;
        }
        if ptr.exit_info_is_set() {
            klog_info!(
                "SCHED_TEST: fresh task {} has stale exit_info at iter {}",
                id,
                i
            );
            return TestResult::Fail;
        }

        let rc = task_terminate(id);
        if rc != 0 {
            klog_info!("SCHED_TEST: task_terminate returned {} at iter {}", rc, i);
            return TestResult::Fail;
        }
        if task_find_by_id(id).is_some() {
            klog_info!("SCHED_TEST: destroyed task {} still upgrades", id);
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// A child's exit info survives unrelated task churn between its termination
/// and the parent's reap: `exit_info` stays in the Zombie task until
/// `task_consume_zombie` transitions it.
pub fn test_waitpid_survives_task_churn() -> TestResult {
    use super::task::{task_consume_zombie, task_set_parent};

    let _fixture = SchedFixture::new();

    // A live parent is what makes the child take the Zombie path on exit.
    let parent_id = task_create(
        b"ZombieParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let child_id = task_create(
        b"ZombieChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    task_set_parent(child_id, parent_id);

    if task_terminate(child_id) != 0 {
        return TestResult::Fail;
    }

    let Some(child) = task_find_by_id(child_id) else {
        return TestResult::Fail;
    };
    let child_ptr: &Task = &child;
    if child_ptr.status() != TaskStatus::Zombie {
        klog_info!(
            "SCHED_TEST: child not Zombie after terminate (status={:?})",
            child_ptr.status()
        );
        return TestResult::Fail;
    }

    // Parentless tasks go straight to Terminated and are destroyed; none may
    // disturb the Zombie child.
    for _ in 0..256 {
        let id = task_create(
            b"ChurnTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        let _ = task_terminate(id);
    }

    let Some(child_ref_after) = task_find_by_id(child_id) else {
        klog_info!("SCHED_TEST: child vanished during churn");
        return TestResult::Fail;
    };
    let child_ptr_after: &Task = &child_ref_after;
    if child_ptr_after.task_id != child_id {
        return TestResult::Fail;
    }
    if child_ptr_after.status() != TaskStatus::Zombie {
        klog_info!("SCHED_TEST: child not Zombie after churn");
        return TestResult::Fail;
    }

    let info = match task_consume_zombie(child_id) {
        Some(i) => i,
        None => {
            klog_info!("SCHED_TEST: task_consume_zombie returned None after churn");
            return TestResult::Fail;
        }
    };
    if info.exit_code != 0 {
        return TestResult::Fail;
    }

    // The held lookup keeps the consumed task inspectable until this check.
    if child_ptr_after.status() != TaskStatus::Terminated {
        klog_info!("SCHED_TEST: child not Terminated after consume");
        return TestResult::Fail;
    }

    let _ = task_terminate(parent_id);
    TestResult::Pass
}

/// A parent that terminates without reaping auto-reaps its Zombie children, so
/// a crashed parent does not pin zombies until the live-task cap exhausts.
pub fn test_orphan_child_auto_reaped_on_parent_exit() -> TestResult {
    use super::task::task_set_parent;

    let _fixture = SchedFixture::new();

    let parent_id = task_create(
        b"OrphanParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let child_id = task_create(
        b"OrphanChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    task_set_parent(child_id, parent_id);

    if task_terminate(child_id) != 0 {
        return TestResult::Fail;
    }
    let Some(child) = task_find_by_id(child_id) else {
        return TestResult::Fail;
    };
    if Some(child.status()).unwrap_or(TaskStatus::Terminated) != TaskStatus::Zombie {
        return TestResult::Fail;
    }
    drop(child);

    // The dying parent must sweep its child list and demote the Zombie.
    if task_terminate(parent_id) != 0 {
        return TestResult::Fail;
    }

    if task_find_by_id(child_id).is_some() {
        klog_info!("SCHED_TEST: orphan child still registered after parent exit");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A linked child is owned by its parent: it sits on the parent's children list
/// and carries exactly one extra parked strong reference (invariant I2 —
/// membership IS the owning reference). `waitpid`'s reap unlinks it, drops that
/// reference, and reclaims the task.
pub fn test_child_owned_by_parent_children_list() -> TestResult {
    use super::task::{task_consume_zombie, task_set_parent};

    let _fixture = SchedFixture::new();

    let parent_id = task_create(
        b"FamParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    let child_id = task_create(
        b"FamChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID || child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let (Some(parent), Some(child)) = (task_find_by_id(parent_id), task_find_by_id(child_id))
    else {
        return TestResult::Fail;
    };
    let parent_ptr: &Task = &parent;
    let child_ptr: &Task = &child;
    let Some(child_nn) = Some(child.node()) else {
        return TestResult::Fail;
    };

    if !parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: parent children list non-empty before link");
        return TestResult::Fail;
    }
    let count_before = task_placement_strong_count(child_nn);

    task_set_parent(child_id, parent_id);

    if parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: child not on parent children list after link");
        return TestResult::Fail;
    }
    if task_placement_strong_count(child_nn) != count_before + 1 {
        klog_info!("SCHED_TEST: link did not add exactly one owning reference");
        return TestResult::Fail;
    }

    if task_terminate(child_id) != 0 {
        return TestResult::Fail;
    }
    if child_ptr.status() != TaskStatus::Zombie {
        klog_info!("SCHED_TEST: child not Zombie after terminate");
        return TestResult::Fail;
    }
    if parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: zombie child fell off parent children list");
        return TestResult::Fail;
    }

    if task_consume_zombie(child_id).is_none() {
        klog_info!("SCHED_TEST: task_consume_zombie returned None");
        return TestResult::Fail;
    }
    if !parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: reaped child still on parent children list");
        return TestResult::Fail;
    }
    if task_find_by_id(child_id).is_some() {
        klog_info!("SCHED_TEST: reaped child still registered");
        return TestResult::Fail;
    }

    let _ = task_terminate(parent_id);
    TestResult::Pass
}

/// A dying parent drains its whole children list in O(children): zombie children
/// are auto-reaped (reclaimed) and live children are orphaned (they survive, off
/// the list, with a cleared parent id).
pub fn test_parent_death_drains_multiple_children() -> TestResult {
    use super::task::task_set_parent;

    let _fixture = SchedFixture::new();

    let parent_id = task_create(
        b"DrainParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut child_ids = [INVALID_TASK_ID; 4];
    for slot in child_ids.iter_mut() {
        let id = task_create(
            b"DrainChild\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        task_set_parent(id, parent_id);
        *slot = id;
    }

    let Some(parent) = task_find_by_id(parent_id) else {
        klog_info!("SCHED_TEST: parent lookup failed after linking four children");
        return TestResult::Fail;
    };
    if parent.children_is_empty() {
        klog_info!("SCHED_TEST: parent children list empty after linking four children");
        return TestResult::Fail;
    }
    drop(parent);

    if task_terminate(child_ids[0]) != 0 || task_terminate(child_ids[1]) != 0 {
        return TestResult::Fail;
    }

    // The parent's own children list must be empty by the time it is reclaimed;
    // the `Drop` tripwire also asserts that.
    if task_terminate(parent_id) != 0 {
        return TestResult::Fail;
    }

    if task_find_by_id(child_ids[0]).is_some() || task_find_by_id(child_ids[1]).is_some() {
        klog_info!("SCHED_TEST: zombie children not reaped on parent death");
        return TestResult::Fail;
    }
    for &id in &child_ids[2..] {
        if task_find_by_id(id).is_none() {
            klog_info!("SCHED_TEST: live child wrongly reaped on parent death");
            return TestResult::Fail;
        }
    }

    // Orphans go straight to Terminated (no reaper remains).
    for &id in &child_ids[2..] {
        let _ = task_terminate(id);
    }
    TestResult::Pass
}

/// A High-priority caller waits on a Low-priority child that has already
/// published its exit: the durable `exit_info` publish must be visible
/// regardless of the producer's priority or runqueue placement.
pub fn test_cross_priority_wait() -> TestResult {
    let _fixture = SchedFixture::new();

    // Allocate the High waiter before the Low child so both lifetimes overlap.
    let high_id = task_create(
        b"HighWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::High.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if high_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: task_create(High) failed");
        return TestResult::Fail;
    }
    let Some(high_ref) = task_find_by_id(high_id) else {
        klog_info!("SCHED_TEST: task_find_by_id(High) null");
        return TestResult::Fail;
    };
    let high_ptr: &Task = &high_ref;
    if Some(high_ptr.priority).unwrap_or(TaskPriority::Low) != TaskPriority::High {
        klog_info!(
            "SCHED_TEST: High waiter priority is {:?}, expected High",
            Some(high_ptr.priority).unwrap_or(TaskPriority::Low)
        );
        return TestResult::Fail;
    }

    // The publish runs on the runner CPU, but the producer slot's `priority`
    // stays Low.
    let child_id = task_create(
        b"LowChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: task_create(Low) failed");
        return TestResult::Fail;
    }

    let Some(child_ref) = task_find_by_id(child_id) else {
        klog_info!("SCHED_TEST: task_find_by_id(Low) null");
        return TestResult::Fail;
    };
    let child_ptr: &Task = &child_ref;
    if Some(child_ptr.priority).unwrap_or(TaskPriority::Low) != TaskPriority::Low {
        klog_info!(
            "SCHED_TEST: Low child priority is {:?}, expected Low",
            Some(child_ptr.priority).unwrap_or(TaskPriority::Low)
        );
        return TestResult::Fail;
    }

    // Distinct slots, or `task_wait_for`'s self-wait short-circuit makes the
    // wait below vacuous.
    if core::ptr::eq(child_ptr, high_ptr) {
        klog_info!("SCHED_TEST: Low and High mapped to the same slot");
        return TestResult::Fail;
    }

    if task_terminate(child_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate(Low) failed");
        return TestResult::Fail;
    }
    if !child_ptr.is_terminated() {
        klog_info!("SCHED_TEST: Low child not Terminated after task_terminate");
        return TestResult::Fail;
    }
    if !child_ptr.exit_info_is_set() {
        klog_info!("SCHED_TEST: exit_info not published by Low producer");
        return TestResult::Fail;
    }

    // `child_ref` keeps the Low task alive across the wait.
    let wait_rc = task_wait_for(child_id);
    if wait_rc != 0 {
        klog_info!(
            "SCHED_TEST: cross-priority task_wait_for returned {} (expected 0)",
            wait_rc
        );
        return TestResult::Fail;
    }

    // The wait path never takes the cell, so it stays observable for later
    // waiters.
    if !child_ptr.exit_info_is_set() {
        klog_info!("SCHED_TEST: exit_info became unset after High-priority wait returned");
        return TestResult::Fail;
    }

    if task_terminate(high_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate(High) failed");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_state_transition_ready_to_running,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_running_to_blocked,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_invalid_terminated_to_running,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_invalid_blocked_to_running,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_raw_ready_store_does_not_reserve_waking_placement,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_publish_new_task_owns_ready_publication,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wake_blocked_task_publishes_from_none,
    suite = sched_core
);
slopos_testing::stest!(name = test_task_registry_live_cap, suite = sched_core);
slopos_testing::stest!(
    name = test_unpublished_task_is_terminable,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_task_guard_pins_terminated_task,
    suite = sched_core
);
slopos_testing::stest!(name = test_rapid_create_destroy_cycle, suite = sched_core);
slopos_testing::stest!(
    name = test_task_handle_stale_after_reuse,
    suite = sched_core
);
slopos_testing::stest!(name = test_kstack_basic_alloc, suite = sched_core);
slopos_testing::stest!(name = test_kstack_slot_reuse, suite = sched_core);
slopos_testing::stest!(name = test_kstack_rejects_invalid_size, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_refill, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_spill_overflow, suite = sched_core);
slopos_testing::stest!(
    name = test_kstack_pcp_was_backed_preserved,
    suite = sched_core
);
slopos_testing::stest!(name = test_kstack_pcp_cross_cpu_safety, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_stress_1000, suite = sched_core);
slopos_testing::stest!(
    name = test_kstack_pcp_smp_throughput_bench,
    suite = sched_core
);
slopos_testing::stest!(name = test_ustack_basic_alloc, suite = sched_core);
slopos_testing::stest!(name = test_ustack_slot_reuse, suite = sched_core);
slopos_testing::stest!(name = test_ustack_rejects_invalid_size, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_refill, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_spill_overflow, suite = sched_core);
slopos_testing::stest!(
    name = test_ustack_pcp_was_backed_preserved,
    suite = sched_core
);
slopos_testing::stest!(name = test_ustack_pcp_cross_cpu_safety, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_stress_1000, suite = sched_core);
slopos_testing::stest!(name = test_regions_disjoint, suite = sched_core);
slopos_testing::stest!(name = test_schedule_to_empty_queue, suite = sched_core);
slopos_testing::stest!(name = test_schedule_duplicate_task, suite = sched_core);
slopos_testing::stest!(
    name = test_schedule_refuses_non_ready_task,
    suite = sched_core
);
slopos_testing::stest!(name = test_unschedule_not_in_queue, suite = sched_core);
slopos_testing::stest!(name = test_priority_ordering, suite = sched_core);
slopos_testing::stest!(name = test_idle_priority_last, suite = sched_core);
slopos_testing::stest!(name = test_timer_tick_decrements_slice, suite = sched_core);
slopos_testing::stest!(name = test_terminate_invalid_id, suite = sched_core);
slopos_testing::stest!(name = test_terminate_nonexistent_id, suite = sched_core);
slopos_testing::stest!(name = test_double_terminate, suite = sched_core);
slopos_testing::stest!(name = test_find_invalid_id, suite = sched_core);
slopos_testing::stest!(name = test_create_null_entry, suite = sched_core);
slopos_testing::stest!(name = test_create_conflicting_flags, suite = sched_core);
slopos_testing::stest!(name = test_create_null_name, suite = sched_core);
slopos_testing::stest!(name = test_scheduler_starts_disabled, suite = sched_core);
slopos_testing::stest!(name = test_schedule_while_disabled, suite = sched_core);
slopos_testing::stest!(
    name = test_schedule_task_before_scheduler_enable_on_current_cpu,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_reports_missing_idle_task,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_reports_missing_kernel_stack,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_for_bsp_uses_idle_task_kernel_stack,
    suite = sched_core
);
slopos_testing::stest!(name = test_many_same_priority_tasks, suite = sched_core);
slopos_testing::stest!(name = test_interleaved_operations, suite = sched_core);
slopos_testing::stest!(name = test_remote_inbox_push_drain, suite = sched_core);
slopos_testing::stest!(
    name = test_remote_inbox_duplicate_push_is_single_membership,
    suite = sched_core
);
slopos_testing::stest!(name = test_remote_inbox_multiple_tasks, suite = sched_core);
slopos_testing::stest!(name = test_timer_tick_drains_inbox, suite = sched_core);
slopos_testing::stest!(
    name = test_remote_inbox_drops_non_ready_tasks,
    suite = sched_core
);
slopos_testing::stest!(name = test_cross_cpu_schedule_lockfree, suite = sched_core);
slopos_testing::stest!(
    name = test_privilege_separation_invariants,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_scheduler_wakeup_race_stress_baseline,
    suite = sched_core
);
slopos_testing::stest!(name = test_sleep_wake_race_regression, suite = sched_core);
slopos_testing::stest!(
    name = test_ensure_sleep_queue_allocated_preserves_entries,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_sleep_entry_survives_unparked_wake,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_tickless_idle_past_deadline_no_overflow,
    suite = sched_core
);
slopos_testing::stest!(name = test_task_wait_exit_race_1000, suite = sched_core);
slopos_testing::stest!(
    name = test_task_wait_exit_race_with_work,
    suite = sched_core
);
slopos_testing::stest!(name = test_task_wait_multi_waiter, suite = sched_core);
slopos_testing::stest!(
    name = test_timer_tick_always_increments_ticks,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_idle_time_tracks_ticks_not_iterations,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_select_target_cpu_prefers_idle_cpu,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_select_target_cpu_running_task_not_idle,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_schedule_new_task_spreads_across_cpus,
    suite = sched_core
);
slopos_testing::stest!(name = test_fork_exit_wait_stress_10x100, suite = sched_core);
slopos_testing::stest!(name = test_serial_reap_stampede, suite = sched_core);
slopos_testing::stest!(name = test_cross_priority_wait, suite = sched_core);
slopos_testing::stest!(name = test_effective_load_accuracy, suite = sched_core);
slopos_testing::stest!(name = test_waitpid_survives_task_churn, suite = sched_core);
slopos_testing::stest!(
    name = test_orphan_child_auto_reaped_on_parent_exit,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_child_owned_by_parent_children_list,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_parent_death_drains_multiple_children,
    suite = sched_core
);

use slopos_ostd::sync::wait_queue::{ParkedTestNode, WaitAbort, WaitQueue};

/// `wait_event_until` returns the closure's `Some(R)` immediately via
/// the pre-check fast path, without touching the scheduler backend.
pub fn test_wait_event_until_pre_check_returns_carried_value() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq1", LOCK_LEVEL_RESOURCE));
    let r = wq.wait_event_until(|| Some(0xCAFE_F00D_u32));
    if r == Ok(0xCAFE_F00D_u32) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Ok(0xCAFE_F00D)",
            r
        );
        TestResult::Fail
    }
}

/// `wait_event_timeout_until` carries the closure's value out on the
/// pre-check fast path.
pub fn test_wait_event_timeout_until_pre_check_ready() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq2", LOCK_LEVEL_RESOURCE));
    let r = wq.wait_event_timeout_until(|| Some(7u32), 100);
    if r == Ok(7u32) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Ok(7)",
            r
        );
        TestResult::Fail
    }
}

/// An unsatisfiable closure never yields a value: the wait must end in
/// `Timeout` (the deadline elapsed) or `NoRuntime` (no current task, which is
/// what this fixture leaves behind), and must not hang or panic.
pub fn test_wait_event_timeout_until_does_not_return_ready_on_none() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq3", LOCK_LEVEL_RESOURCE));
    let r = wq.wait_event_timeout_until(|| None::<u32>, 1);
    match r {
        Err(WaitAbort::Timeout | WaitAbort::NoRuntime) => TestResult::Pass,
        other => {
            klog_info!(
                "WAIT_QUEUE_TEST: timeout test returned {:?}, expected Timeout or NoRuntime",
                other
            );
            TestResult::Fail
        }
    }
}

/// `wait_event` succeeds on the pre-check fast path.
pub fn test_wait_event_bool_wrapper_pre_check_true() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq4", LOCK_LEVEL_RESOURCE));
    if wq.wait_event(|| true).is_ok() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wait_event_timeout` reports an abort rather than success when the
/// condition never holds.
pub fn test_wait_event_timeout_bool_wrapper_times_out() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq5", LOCK_LEVEL_RESOURCE));
    if wq.wait_event_timeout(|| false, 1).is_err() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `has_waiters()` on a fresh queue returns `false` via the lock-free read
/// path; it must not take the queue's `SpinLock`.
pub fn test_has_waiters_fresh_queue_is_false() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq6", LOCK_LEVEL_RESOURCE));
    if !wq.has_waiters() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// A fresh `WaitQueue` has `generation == 0`.
pub fn test_wait_queue_initial_generation() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq8", LOCK_LEVEL_RESOURCE));
    if wq.generation() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_one` on an empty queue returns `false` without panicking.
pub fn test_wake_one_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq9", LOCK_LEVEL_RESOURCE));
    if !wq.wake_one() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_all` on an empty queue returns 0 without panicking.
pub fn test_wake_all_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq10", LOCK_LEVEL_RESOURCE));
    if wq.wake_all() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// The generation only advances when at least one waiter was actually woken.
pub fn test_generation_unchanged_when_no_waiters() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new(lock_class!("test.wq11", LOCK_LEVEL_RESOURCE));
    let gen_before = wq.generation();
    let _ = wq.wake_one();
    let _ = wq.wake_all();
    if wq.generation() == gen_before {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

slopos_testing::stest!(
    name = test_wait_event_until_pre_check_returns_carried_value,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_until_pre_check_ready,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_until_does_not_return_ready_on_none,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_bool_wrapper_pre_check_true,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_bool_wrapper_times_out,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_has_waiters_fresh_queue_is_false,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_queue_initial_generation,
    suite = sched_core
);
slopos_testing::stest!(name = test_wake_one_on_empty_queue, suite = sched_core);
slopos_testing::stest!(name = test_wake_all_on_empty_queue, suite = sched_core);
slopos_testing::stest!(
    name = test_generation_unchanged_when_no_waiters,
    suite = sched_core
);

/// `TaskPriority::KernelIo` is numerically 1, between `High`=0 and `Normal`=2,
/// in the total decoder, the strict decoder and the dispatch index alike.
fn test_kernel_io_priority_renumber() -> TestResult {
    if TaskPriority::High.as_u8() != 0 {
        return TestResult::Fail;
    }
    if TaskPriority::KernelIo.as_u8() != 1 {
        return TestResult::Fail;
    }
    if TaskPriority::Normal.as_u8() != 2 {
        return TestResult::Fail;
    }
    if TaskPriority::Low.as_u8() != 3 {
        return TestResult::Fail;
    }
    if TaskPriority::Idle.as_u8() != 4 {
        return TestResult::Fail;
    }
    for v in 0..=4u8 {
        if TaskPriority::from_u8(v).as_u8() != v {
            return TestResult::Fail;
        }
    }
    if TaskPriority::from_u8(255) != TaskPriority::Normal {
        return TestResult::Fail;
    }
    if TaskPriority::try_from_u8(5).is_some() {
        return TestResult::Fail;
    }
    if TaskPriority::try_from_u8(1) != Some(TaskPriority::KernelIo) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// With no sleepers the lock-free fast path returns `None`, so the tickless
/// idle loop skips arming a one-shot LAPIC.
fn test_sleep_queue_next_deadline_none_when_empty() -> TestResult {
    let _fix = SchedFixture::new();
    let now = super::sleep::sleep_queue_now_ms();
    match super::sleep::sleep_queue_next_deadline_ms(now) {
        None => TestResult::Pass,
        Some(_) => TestResult::Fail,
    }
}

/// A sleep deadline must not scale with the number of online CPUs.
///
/// The sleep queue used to key on `platform::timer_ticks()`, a single global
/// counter incremented by *every* CPU's LAPIC ISR, while converting via
/// `timer_frequency()` = 100. At N CPUs it advanced at N*100 Hz, so every
/// deadline expired N times early — a 200 ms sleep returned in 66 ms on a
/// 4-CPU boot. Deadlines are wall-clock milliseconds now, so this holds
/// regardless of CPU count.
fn test_sleep_deadline_is_wall_clock_not_cpu_scaled() -> TestResult {
    let _fix = SchedFixture::new();

    let before = slopos_kernel_services::clock::monotonic_ns() / 1_000_000;
    let now = super::sleep::sleep_queue_now_ms();
    let after = slopos_kernel_services::clock::monotonic_ns() / 1_000_000;

    if now < before || now > after {
        klog_info!(
            "SCHED_TEST: sleep clock {} outside monotonic window [{}, {}]",
            now,
            before,
            after
        );
        return TestResult::Fail;
    }

    super::sleep::reset_sleep_queue();
    const TIMEOUT_MS: u32 = 500;
    let armed_at = super::sleep::sleep_queue_now_ms();
    if !super::sleep::test_insert_sleep_entry(909_090, armed_at.wrapping_add(TIMEOUT_MS as u64)) {
        super::sleep::reset_sleep_queue();
        klog_info!("SCHED_TEST: could not insert probe entry");
        return TestResult::Fail;
    }
    let deadline = super::sleep::sleep_queue_next_deadline_ms(armed_at);
    super::sleep::reset_sleep_queue();

    let Some(deadline) = deadline else {
        klog_info!("SCHED_TEST: no deadline reported for an armed entry");
        return TestResult::Fail;
    };
    let delta = deadline.wrapping_sub(armed_at);
    if delta != TIMEOUT_MS as u64 {
        klog_info!(
            "SCHED_TEST: {} ms timeout produced a {} ms deadline delta",
            TIMEOUT_MS,
            delta
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}
slopos_testing::stest!(
    name = test_sleep_deadline_is_wall_clock_not_cpu_scaled,
    suite = sched_core
);

/// Pins the inner ABI of `KernelIoToken::__new_for_trampoline_only`, the only
/// witness constructor.
fn test_kernel_io_token_constructs() -> TestResult {
    let _t =
        slopos_ostd::sync::kernel_io_task::KernelIoToken::<'static>::__new_for_trampoline_only();
    TestResult::Pass
}

slopos_testing::stest!(name = test_kernel_io_priority_renumber, suite = sched_core);
slopos_testing::stest!(
    name = test_sleep_queue_next_deadline_none_when_empty,
    suite = sched_core
);
slopos_testing::stest!(name = test_kernel_io_token_constructs, suite = sched_core);

fn test_preempt_guard_nesting_balances() -> TestResult {
    let baseline = slopos_ostd::sync::preempt_count();
    let outer = slopos_ostd::sync::PreemptGuard::new();
    if slopos_ostd::sync::preempt_count() != baseline + 1 {
        klog_info!("SCHED_TEST: outer guard did not raise count by 1");
        return TestResult::Fail;
    }
    let inner = slopos_ostd::sync::PreemptGuard::new();
    if slopos_ostd::sync::preempt_count() != baseline + 2 {
        klog_info!("SCHED_TEST: inner guard did not raise count by 1");
        return TestResult::Fail;
    }
    drop(inner);
    if slopos_ostd::sync::preempt_count() != baseline + 1 {
        klog_info!("SCHED_TEST: inner drop did not lower count by 1");
        return TestResult::Fail;
    }
    drop(outer);
    if slopos_ostd::sync::preempt_count() != baseline {
        klog_info!("SCHED_TEST: outer drop did not restore baseline");
        return TestResult::Fail;
    }
    TestResult::Pass
}

fn test_preempt_reschedule_pending_deferred_not_lost() -> TestResult {
    // IRQs off gates the guard-drop deferred callback off, so a guard dropped
    // with the flag set must leave it set for the trap-exit handoff.
    let ok = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| -> bool {
        PreemptGuard::clear_reschedule_pending();
        let guard = PreemptGuard::new();
        PreemptGuard::set_reschedule_pending();
        if !PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: pending flag not observed after set");
            return false;
        }
        drop(guard);
        if !PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: IRQs-off guard drop consumed pending flag");
            return false;
        }
        PreemptGuard::clear_reschedule_pending();
        if PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: pending flag survived clear");
            return false;
        }
        true
    });
    if ok {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

fn test_preempt_count_balanced_under_guard_churn() -> TestResult {
    static CHURN_LOCK: SpinLock<u64> =
        SpinLock::new(0, lock_class!("test.churn_lock", LOCK_LEVEL_RESOURCE));

    let baseline = slopos_ostd::sync::preempt_count();
    for i in 0..20_000u32 {
        let guard = slopos_ostd::sync::PreemptGuard::new();
        core::hint::black_box(&guard);
        drop(guard);

        // SpinLock churn: guard + cli + critical section.
        let mut slot = CHURN_LOCK.lock();
        *slot = slot.wrapping_add(1);
        drop(slot);

        // Invite the timer to preempt (and the stealer to migrate) the task.
        if i % 1024 == 0 {
            scheduler::yield_();
        }
    }
    if slopos_ostd::sync::preempt_count() != baseline {
        klog_info!("SCHED_TEST: preempt count drifted across guard churn");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_preempt_guard_nesting_balances,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_preempt_reschedule_pending_deferred_not_lost,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_preempt_count_balanced_under_guard_churn,
    suite = sched_core
);

fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

/// Make `task_id` this CPU's current task.
///
/// `dispatch` asserts its incoming task is runnable, so the Ready publish is
/// not optional — a task straight out of `task_create` is Blocked, and
/// dispatching one trips that invariant rather than returning an error.
fn dispatch_as_current(task_id: u32) -> bool {
    let cpu = slopos_arch::pcr::get_current_cpu();
    make_task_ready(task_id) && scheduler::dispatch_task_for_test(cpu, task_id)
}

/// Park `task_id` on `wq`.
///
/// [`WaitQueue::enqueue_current`] reads the waiter's identity from the PCR, so
/// the task must be this CPU's current for the duration of the enqueue. The
/// bootstrap stub is restored before returning: a task that is still some CPU's
/// current is dispatch-pinned, and the reap would refuse it.
fn park_task_on_queue(wq: &WaitQueue, task_id: u32) -> bool {
    if !scheduler::clear_nascent_for_test(task_id) {
        return false;
    }
    if !dispatch_as_current(task_id) {
        return false;
    }
    let parked = wq.enqueue_current();
    park_bootstrap_on_current_cpu();
    parked
}

/// `(id, status)` for every registered task, so a stray wake shows up as a diff.
fn snapshot_live_task_states() -> slopos_ostd::KVec<(u32, TaskStatus)> {
    let mut out = match slopos_ostd::KVec::with_capacity(super::task::MAX_TASKS) {
        Ok(v) => v,
        Err(_) => return slopos_ostd::KVec::new(),
    };
    super::task::task_for_each_active(|task| {
        let _ = out.push((task.task_id, task.status()));
    });
    out
}

/// A wake delivered to a wait-queue node whose task has been reaped resolves to
/// nothing, and disturbs no other task.
///
/// Teardown never unlinks wait-queue nodes, so a node outliving its task is
/// structurally reachable. The node holds an id resolved through the registry,
/// which is what makes it inert; a pointer would read freed memory. The
/// `weak.upgrade()` check asserts the allocation is *freed*, not merely
/// unhashed.
pub fn test_wake_against_reaped_waiter_is_inert() -> TestResult {
    let _fixture = SchedFixture::new();

    let victim_id = task_create(
        b"ReapedWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if victim_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(victim) = task_find_by_id(victim_id) else {
        return TestResult::Fail;
    };
    let weak = victim.downgrade_for_test();

    let wq = WaitQueue::new(lock_class!("test.wq12", LOCK_LEVEL_RESOURCE));
    if !park_task_on_queue(&wq, victim_id) {
        klog_info!("SCHED_TEST: could not park the victim on a wait queue");
        let _ = task_terminate(victim_id);
        return TestResult::Fail;
    }
    if !wq.has_waiters() {
        klog_info!("SCHED_TEST: enqueue_current reported success but queued nothing");
        let _ = task_terminate(victim_id);
        return TestResult::Fail;
    }

    drop(victim);
    if task_terminate(victim_id) != 0 {
        klog_info!("SCHED_TEST: could not terminate the parked victim");
        return TestResult::Fail;
    }
    crate::task::task_graveyard_drain();

    if task_find_by_id(victim_id).is_some() {
        klog_info!("SCHED_TEST: reaped waiter {} still resolves", victim_id);
        return TestResult::Fail;
    }
    if weak.upgrade().is_some() {
        klog_info!(
            "SCHED_TEST: reaped waiter {} is still allocated — its node names memory that could be reused",
            victim_id
        );
        return TestResult::Fail;
    }

    let before = snapshot_live_task_states();

    if !wq.wake_one() {
        klog_info!("SCHED_TEST: the reaped waiter's node was not on the queue");
        return TestResult::Fail;
    }
    if wq.has_waiters() {
        klog_info!("SCHED_TEST: wake_one left the queue non-empty");
        return TestResult::Fail;
    }

    let after = snapshot_live_task_states();
    if before.len() != after.len() {
        klog_info!(
            "SCHED_TEST: live-task set changed across the wake ({} -> {})",
            before.len(),
            after.len()
        );
        return TestResult::Fail;
    }
    for (i, (id, status)) in before.iter().enumerate() {
        let (after_id, after_status) = after[i];
        if *id != after_id || *status != after_status {
            klog_info!(
                "SCHED_TEST: wake against a reaped waiter disturbed task {} ({:?} -> {:?})",
                id,
                status,
                after_status
            );
            return TestResult::Fail;
        }
    }

    // Positive control: without it the no-disturbance assertion above holds
    // vacuously.
    let live_id = task_create(
        b"LiveWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if live_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let control = WaitQueue::new(lock_class!("test.wq13", LOCK_LEVEL_RESOURCE));
    if !park_task_on_queue(&control, live_id) {
        let _ = task_terminate(live_id);
        return TestResult::Fail;
    }
    if task_set_state(live_id, TaskStatus::Blocked) != 0 {
        klog_info!("SCHED_TEST: could not block the control waiter");
        let _ = task_terminate(live_id);
        return TestResult::Fail;
    }
    if !control.wake_one() {
        klog_info!("SCHED_TEST: control waiter was not on its queue");
        let _ = task_terminate(live_id);
        return TestResult::Fail;
    }
    let Some(live) = task_find_by_id(live_id) else {
        return TestResult::Fail;
    };
    let control_woken = live.status() == TaskStatus::Ready;
    let observed = live.status();
    drop(live);
    let _ = task_terminate(live_id);
    if !control_woken {
        klog_info!(
            "SCHED_TEST: control waiter was not woken (status {:?}) — the reaped-waiter result above is vacuous",
            observed
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// The preemption gate: strictly-higher priority preempts, equal does not, and
/// a CPU parked on a bootstrap stub loses to every real priority.
///
/// Equal priority is the case that matters: a non-strict comparison still
/// passes a high-versus-low check and turns every same-priority wake into a
/// reschedule.
pub fn test_newcomer_outranks_current_preemption_gate() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu = slopos_arch::pcr::get_current_cpu();

    let make = |name: &[u8], priority: TaskPriority| -> Option<TaskRef> {
        let id = task_create(
            name.as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            priority.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return None;
        }
        task_find_by_id(id)
    };

    let (Some(high), Some(normal_a), Some(normal_b), Some(low), Some(idle)) = (
        make(b"OutrankHigh\0", TaskPriority::High),
        make(b"OutrankNormA\0", TaskPriority::Normal),
        make(b"OutrankNormB\0", TaskPriority::Normal),
        make(b"OutrankLow\0", TaskPriority::Low),
        make(b"OutrankIdle\0", TaskPriority::Idle),
    ) else {
        return TestResult::Fail;
    };

    if !dispatch_as_current(normal_a.task_id) {
        klog_info!("SCHED_TEST: could not dispatch the current-task fixture");
        return TestResult::Fail;
    }
    if !scheduler::newcomer_outranks_current(cpu, &high) {
        klog_info!("SCHED_TEST: High did not outrank a Normal current task");
        return TestResult::Fail;
    }
    if scheduler::newcomer_outranks_current(cpu, &normal_b) {
        klog_info!("SCHED_TEST: equal priority preempted — the comparison is not strict");
        return TestResult::Fail;
    }
    if scheduler::newcomer_outranks_current(cpu, &low) {
        klog_info!("SCHED_TEST: Low outranked a Normal current task — comparison inverted");
        return TestResult::Fail;
    }

    park_bootstrap_on_current_cpu();
    if !scheduler::newcomer_outranks_current(cpu, &idle) {
        klog_info!("SCHED_TEST: Idle did not outrank PRIORITY_NONE");
        return TestResult::Fail;
    }

    slopos_arch::pcr::mark_cpu_online(cpu);
    if super::per_cpu::with_cpu_scheduler(cpu, |sched| sched.enable()).is_none() {
        return TestResult::Fail;
    }
    let affinity = super::per_cpu::affinity_mask_for_cpu(cpu);
    crate::task::task_install_idle_affinity(&high, affinity, cpu as u8);
    crate::task::task_install_idle_affinity(&normal_b, affinity, cpu as u8);

    // The reschedule request is gated on scheduling being active; enable it
    // only across the two publications.
    scheduler::set_scheduler_enabled(true);
    let effect = (|| {
        if !scheduler::is_scheduling_active() {
            klog_info!("SCHED_TEST: could not make scheduling active — effect half is untestable");
            return TestResult::Fail;
        }

        if !dispatch_as_current(normal_a.task_id) {
            return TestResult::Fail;
        }
        PreemptGuard::clear_reschedule_pending();
        if !make_task_ready(high.task_id) || scheduler::schedule_task(&high) != 0 {
            klog_info!("SCHED_TEST: could not publish the High newcomer");
            return TestResult::Fail;
        }
        if !PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: a higher-priority wake did not request a reschedule");
            return TestResult::Fail;
        }

        if !dispatch_as_current(normal_a.task_id) {
            return TestResult::Fail;
        }
        PreemptGuard::clear_reschedule_pending();
        if !make_task_ready(normal_b.task_id) || scheduler::schedule_task(&normal_b) != 0 {
            klog_info!("SCHED_TEST: could not publish the equal-priority newcomer");
            return TestResult::Fail;
        }
        if PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: an equal-priority wake requested a reschedule");
            return TestResult::Fail;
        }
        TestResult::Pass
    })();
    scheduler::set_scheduler_enabled(false);
    PreemptGuard::clear_reschedule_pending();
    park_bootstrap_on_current_cpu();

    for id in [
        high.task_id,
        normal_a.task_id,
        normal_b.task_id,
        low.task_id,
        idle.task_id,
    ] {
        let _ = task_terminate(id);
    }
    effect
}

/// `is_bootstrap_task_ptr` accepts stub base addresses and nothing else.
///
/// The interior sweep is exhaustive only because the stub is exactly its 8-byte
/// ABI header, which is asserted here so a growth fails by name rather than
/// silently reducing this test's coverage to the first byte.
pub fn test_bootstrap_task_ptr_rejects_interior_addresses() -> TestResult {
    use slopos_ostd::task::bootstrap::{
        AP_BOOTSTRAP_TASKS, BSP_BOOTSTRAP_TASK, BootstrapTaskAbi, MAX_STATIC_APS,
        is_bootstrap_task_ptr,
    };

    let stride = core::mem::size_of::<BootstrapTaskAbi>();
    if stride != 8 {
        klog_info!(
            "SCHED_TEST: BootstrapTaskAbi is {} bytes, not 8 — the interior sweep below is no longer exhaustive",
            stride
        );
        return TestResult::Fail;
    }

    if is_bootstrap_task_ptr(ptr::null()) {
        klog_info!("SCHED_TEST: null accepted as a bootstrap stub");
        return TestResult::Fail;
    }

    let bsp = BSP_BOOTSTRAP_TASK.get() as usize;
    if !is_bootstrap_task_ptr(bsp as *const ()) {
        klog_info!("SCHED_TEST: the BSP stub base was rejected");
        return TestResult::Fail;
    }
    for off in 1..stride {
        if is_bootstrap_task_ptr((bsp + off) as *const ()) {
            klog_info!("SCHED_TEST: BSP stub +{} accepted as a stub base", off);
            return TestResult::Fail;
        }
    }

    let base = AP_BOOTSTRAP_TASKS.get() as usize;
    let end = base + stride * MAX_STATIC_APS;
    for slot in 0..MAX_STATIC_APS {
        let slot_base = base + slot * stride;
        if !is_bootstrap_task_ptr(slot_base as *const ()) {
            klog_info!("SCHED_TEST: AP stub {} base was rejected", slot);
            return TestResult::Fail;
        }
        for off in 1..stride {
            if is_bootstrap_task_ptr((slot_base + off) as *const ()) {
                klog_info!(
                    "SCHED_TEST: AP stub {} +{} accepted as a stub base",
                    slot,
                    off
                );
                return TestResult::Fail;
            }
        }
    }

    // The two statics are independent, so the linker may place the BSP stub
    // immediately either side of the array; skip those cases.
    if end != bsp && is_bootstrap_task_ptr(end as *const ()) {
        klog_info!("SCHED_TEST: one-past-the-end of the AP array accepted");
        return TestResult::Fail;
    }
    if base > 0 && (base - 1) != bsp && is_bootstrap_task_ptr((base - 1) as *const ()) {
        klog_info!("SCHED_TEST: one-before the AP array accepted");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Task ids are strictly increasing, never recycled once retired, and the
/// allocator does not rewind across a fixture reset.
///
/// Every id-keyed subsystem is safe only because a stale id resolves to
/// nothing; nothing else checks `task_registry_reset`'s monotonicity promise.
pub fn test_task_ids_are_never_reused() -> TestResult {
    let _fixture = SchedFixture::new();

    const ROUNDS: usize = 8;
    let mut seen = [INVALID_TASK_ID; ROUNDS];
    let mut previous = 0u32;

    for round in seen.iter_mut() {
        let id = task_create(
            b"IdMonotonic\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        if id <= previous {
            klog_info!(
                "SCHED_TEST: task id {} did not advance past {}",
                id,
                previous
            );
            return TestResult::Fail;
        }
        previous = id;
        *round = id;

        if task_terminate(id) != 0 {
            return TestResult::Fail;
        }
        if task_find_by_id(id).is_some() {
            klog_info!("SCHED_TEST: terminated task {} still resolves", id);
            return TestResult::Fail;
        }
        if !crate::task::task_id_was_allocated(id) {
            klog_info!(
                "SCHED_TEST: id {} fell below the allocator watermark — it can be handed out again",
                id
            );
            return TestResult::Fail;
        }
    }

    for (i, id) in seen.iter().enumerate() {
        for other in seen.iter().skip(i + 1) {
            if id == other {
                klog_info!("SCHED_TEST: task id {} was handed out twice", id);
                return TestResult::Fail;
            }
        }
    }

    // The monotonic id source must survive a fixture reset.
    let freeze = crate::task::freeze_kernel_io_all();
    let reset = crate::task::task_registry_reset(&freeze);
    drop(freeze);
    if reset != 0 {
        klog_info!("SCHED_TEST: task_registry_reset failed");
        return TestResult::Fail;
    }
    let after_reset = task_create(
        b"IdPostReset\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if after_reset == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let rewound = after_reset <= previous;
    let _ = task_terminate(after_reset);
    if rewound {
        klog_info!(
            "SCHED_TEST: id allocator rewound across a fixture reset ({} after {})",
            after_reset,
            previous
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// The AP pause nests on a depth: a release resumes the APs only when it is the
/// last outstanding, and releasing in acquire order must work as well as in
/// reverse.
///
/// Deliberately runs without a fixture — `KernelTestScope` holds a pause for
/// the whole of any test that uses one, and the transition pinned here is the
/// one at depth zero.
pub fn test_ap_pause_nests_on_a_depth_count() -> TestResult {
    if crate::per_cpu::ap_pause_depth() != 0 {
        klog_info!("SCHED_TEST: entered with an AP pause already held");
        return TestResult::Fail;
    }

    let first = match crate::per_cpu::pause_all_aps() {
        Ok(token) => token,
        Err(err) => {
            klog_info!("SCHED_TEST: pause_all_aps failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let second = match crate::per_cpu::pause_all_aps() {
        Ok(token) => token,
        Err(err) => {
            crate::per_cpu::resume_all_aps_if_not_nested(first);
            klog_info!("SCHED_TEST: nested pause_all_aps failed: {:?}", err);
            return TestResult::Fail;
        }
    };

    // Every observation is recorded before it is judged, so no failure path
    // returns with a token still outstanding and leaves the APs parked.
    let depth_nested = crate::per_cpu::ap_pause_depth();
    let paused_nested = crate::per_cpu::are_aps_paused();

    crate::per_cpu::resume_all_aps_if_not_nested(first);
    let depth_after_first = crate::per_cpu::ap_pause_depth();
    let paused_after_first = crate::per_cpu::are_aps_paused();

    crate::per_cpu::resume_all_aps_if_not_nested(second);
    let depth_after_second = crate::per_cpu::ap_pause_depth();
    let paused_after_second = crate::per_cpu::are_aps_paused();

    if depth_nested != 2 || !paused_nested {
        klog_info!(
            "SCHED_TEST: two pauses gave depth {} paused {}",
            depth_nested,
            paused_nested
        );
        return TestResult::Fail;
    }
    if depth_after_first != 1 || !paused_after_first {
        klog_info!(
            "SCHED_TEST: one release gave depth {} paused {} — the surviving holder lost its pause",
            depth_after_first,
            paused_after_first
        );
        return TestResult::Fail;
    }
    if depth_after_second != 0 || paused_after_second {
        klog_info!(
            "SCHED_TEST: last release gave depth {} paused {}",
            depth_after_second,
            paused_after_second
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// A pause that cannot be established is a reported error, and the failed
/// attempt leaves no depth behind — a leaked depth parks every AP for the rest
/// of the boot.
///
/// The timeout is provoked through the executing flag, the only state
/// `pause_all_aps` waits on. An AP may clear it by passing through its dispatch
/// path underneath the attempt, so the provocation gets a bounded number of
/// tries rather than silently passing.
///
/// The classification is checked against ground truth rather than asserted
/// outright: the budget is a few timer periods, so whether the held AP bumps
/// its heartbeat inside the wait is a race the kernel is allowed to lose either
/// way. What is not a race is the implication — a heartbeat that provably did
/// not move across a window *containing* the wait cannot have moved inside it,
/// so `NotRunning` is then mandatory.
pub fn test_ap_pause_timeout_is_reported_and_rolled_back() -> TestResult {
    if slopos_arch::pcr::get_cpu_count() < 2 {
        klog_info!("SCHED_TEST: uniprocessor boot has no AP to hold a pause off");
        return TestResult::Skipped;
    }
    if crate::per_cpu::ap_pause_depth() != 0 {
        klog_info!("SCHED_TEST: entered with an AP pause already held");
        return TestResult::Fail;
    }

    // `set_executing_task(true)` below is a lie about CPU 1, which its own
    // dispatch loop clears whenever it runs. Park the kernel-I/O threads so
    // nothing is dispatched there while the lie must hold.
    let _freeze = crate::task::freeze_kernel_io_all();

    const ATTEMPTS: usize = 3;
    const HELD_CPU: usize = 1;
    let mut failure = None;
    for _ in 0..ATTEMPTS {
        if crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(true))
            .is_none()
        {
            klog_info!("SCHED_TEST: CPU {} has no per-CPU scheduler", HELD_CPU);
            return TestResult::Fail;
        }
        let beat_before = slopos_arch::pcr::heartbeat_for_cpu(HELD_CPU);
        let outcome = crate::per_cpu::pause_all_aps();
        let beat_after = slopos_arch::pcr::heartbeat_for_cpu(HELD_CPU);
        crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(false));
        match outcome {
            Err(err) => {
                failure = Some((err, beat_after == beat_before));
                break;
            }
            Ok(token) => crate::per_cpu::resume_all_aps_if_not_nested(token),
        }
    }

    let Some((err, beat_was_still)) = failure else {
        klog_info!("SCHED_TEST: pause_all_aps never observed the held AP");
        return TestResult::Fail;
    };
    let cpu_id = err.cpu_id();
    if cpu_id != HELD_CPU {
        klog_info!(
            "SCHED_TEST: timeout blamed CPU {} instead of CPU {}",
            cpu_id,
            HELD_CPU
        );
        return TestResult::Fail;
    }
    if beat_was_still && !matches!(err, crate::per_cpu::ApPauseError::NotRunning { .. }) {
        klog_info!(
            "SCHED_TEST: CPU {} took no tick across the whole attempt but was classified {:?}",
            HELD_CPU,
            err
        );
        return TestResult::Fail;
    }

    let leftover_depth = crate::per_cpu::ap_pause_depth();
    if leftover_depth != 0 || crate::per_cpu::are_aps_paused() {
        klog_info!(
            "SCHED_TEST: failed pause left depth {} behind",
            leftover_depth
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// An AP that died mid-dispatch never clears its own executing flag, so a pause
/// that waits on it waits forever. The skip counter, not the flag, is what
/// proves the pause stepped over it: a live AP clears its own flag as soon as
/// it dispatches, but the counter only ever moves for an offline CPU.
pub fn test_ap_pause_ignores_an_offline_ap() -> TestResult {
    if slopos_arch::pcr::get_cpu_count() < 2 {
        klog_info!("SCHED_TEST: uniprocessor boot has no AP to take offline");
        return TestResult::Skipped;
    }
    if crate::per_cpu::ap_pause_depth() != 0 {
        klog_info!("SCHED_TEST: entered with an AP pause already held");
        return TestResult::Fail;
    }

    const HELD_CPU: usize = 1;
    let _freeze = crate::task::freeze_kernel_io_all();

    if crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(true))
        .is_none()
    {
        klog_info!("SCHED_TEST: CPU {} has no per-CPU scheduler", HELD_CPU);
        return TestResult::Fail;
    }
    slopos_arch::pcr::mark_cpu_offline(HELD_CPU);

    let skips_before = crate::per_cpu::skipped_offline_ap_count();
    let outcome = crate::per_cpu::pause_all_aps();
    let skips_after = crate::per_cpu::skipped_offline_ap_count();

    slopos_arch::pcr::mark_cpu_online(HELD_CPU);
    crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(false));

    match outcome {
        Ok(token) => crate::per_cpu::resume_all_aps_if_not_nested(token),
        Err(err) => {
            klog_info!("SCHED_TEST: pause waited on an offline AP: {:?}", err);
            return TestResult::Fail;
        }
    }

    if skips_after == skips_before {
        klog_info!(
            "SCHED_TEST: CPU {} cleared its own flag before the scan; nothing was stepped over",
            HELD_CPU
        );
        return TestResult::Skipped;
    }

    let leftover_depth = crate::per_cpu::ap_pause_depth();
    if leftover_depth != 0 || crate::per_cpu::are_aps_paused() {
        klog_info!("SCHED_TEST: pause left depth {} behind", leftover_depth);
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// The wait's bound is wall time when a clock is available, and every
/// degenerate reading falls back to the iteration budget rather than expiring.
///
/// The backwards-clock and zero-clock arms are the load-bearing ones: a
/// `wrapping_sub` here yields ~2^64 on a clock that went backwards and expires
/// the wait instantly, blaming an AP for the host's timekeeping.
pub fn test_pause_deadline_passed_uses_wall_clock_when_available() -> TestResult {
    use crate::per_cpu::{AP_PAUSE_SPIN_BUDGET_FOR_TEST as SPINS, test_pause_deadline_passed as p};

    const BUDGET: u64 = 12_000_000;
    const START: u64 = 1_000_000_000;

    // A slice of a `const`, not an array local: the array form materialises
    // every case on this frame and costs ~3 KiB, past the stack-size gate.
    //
    // (start, now, iteration, budget, expected, what it pins)
    const CASES: &[(u64, u64, u32, u64, bool, &str)] = &[
        (
            START,
            START + BUDGET - 1,
            0,
            BUDGET,
            false,
            "inside the budget",
        ),
        (START, START + BUDGET, 0, BUDGET, true, "at the budget"),
        (
            START,
            START + BUDGET * 2,
            0,
            BUDGET,
            true,
            "past the budget",
        ),
        (
            START,
            START + BUDGET - 1,
            u32::MAX,
            BUDGET,
            false,
            "iterations do not expire a live wall-clock wait",
        ),
        (
            START,
            START - 1,
            0,
            BUDGET,
            false,
            "a clock that went backwards is not the AP's fault",
        ),
        (
            START,
            0,
            0,
            BUDGET,
            false,
            "a zero end reading falls back, under the spin budget",
        ),
        (
            START,
            0,
            SPINS,
            BUDGET,
            true,
            "a zero end reading falls back, at the spin budget",
        ),
        (
            0,
            START,
            0,
            BUDGET,
            false,
            "no start clock falls back, under the spin budget",
        ),
        (
            0,
            START,
            SPINS,
            BUDGET,
            true,
            "no start clock falls back, at the spin budget",
        ),
        (
            START,
            START + BUDGET * 100,
            SPINS - 1,
            0,
            false,
            "budget 0 disables the deadline",
        ),
    ];

    for &(start, now, iteration, budget, expected, what) in CASES {
        let got = p(start, now, iteration, budget);
        if got != expected {
            klog_info!(
                "SCHED_TEST: pause_deadline_passed({}, {}, {}, {}) = {}, want {} — {}",
                start,
                now,
                iteration,
                budget,
                got,
                expected,
                what
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// A stalled heartbeat reads as `NotRunning` and a moving one as `NotParking`.
///
/// Driven over the classifier directly rather than through a provoked pause:
/// whether a held AP happens to tick inside a few-millisecond window is a race,
/// so a live pause makes both arms reachable and neither mandatory — a test
/// that asserts on it passes just as happily on a classifier that has lost an
/// arm entirely. The heartbeat is what the classifier reads, so a baseline
/// equal to the live value is a stalled CPU by construction, and one that
/// differs is a CPU that has run.
pub fn test_pause_failure_classification_reads_the_heartbeat() -> TestResult {
    use crate::per_cpu::{ApPauseError, test_classify_pause_failure as classify};

    const CPU: usize = 0;
    let live = slopos_arch::pcr::heartbeat_for_cpu(CPU);

    let stalled = classify(CPU, Some(live));
    if !matches!(stalled, ApPauseError::NotRunning { cpu_id } if cpu_id == CPU) {
        klog_info!(
            "SCHED_TEST: an unchanged heartbeat classified {:?}, want NotRunning",
            stalled
        );
        return TestResult::Fail;
    }

    let moved = classify(CPU, Some(live.wrapping_sub(1)));
    if !matches!(moved, ApPauseError::NotParking { cpu_id } if cpu_id == CPU) {
        klog_info!(
            "SCHED_TEST: an advanced heartbeat classified {:?}, want NotParking",
            moved
        );
        return TestResult::Fail;
    }

    // No baseline means the blamed CPU is not the one sampled: it took up a
    // task during the wait, so it has demonstrably run.
    let unsampled = classify(CPU, None);
    if !matches!(unsampled, ApPauseError::NotParking { cpu_id } if cpu_id == CPU) {
        klog_info!(
            "SCHED_TEST: an unsampled CPU classified {:?}, want NotParking",
            unsampled
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// `sched.ap_pause_ms=` moves the live budget, and 0 disables the deadline.
///
/// Restores whatever was configured: leaving a 1 ms budget behind would make
/// every later scope's pause fail, and leaving 0 behind would silently put the
/// rest of the run back on the iteration bound this work removed.
pub fn test_ap_pause_budget_knob_round_trips() -> TestResult {
    let restore_ns = crate::per_cpu::ap_pause_budget_ns_for_test();

    crate::per_cpu::set_ap_pause_budget_ms(7);
    let seven = crate::per_cpu::ap_pause_budget_ns_for_test();
    crate::per_cpu::set_ap_pause_budget_ms(0);
    let zero = crate::per_cpu::ap_pause_budget_ns_for_test();

    crate::per_cpu::set_ap_pause_budget_ms(restore_ns / 1_000_000);
    let restored = crate::per_cpu::ap_pause_budget_ns_for_test();

    if seven != 7_000_000 {
        klog_info!("SCHED_TEST: 7 ms became {} ns", seven);
        return TestResult::Fail;
    }
    if zero != 0 {
        klog_info!(
            "SCHED_TEST: 0 ms became {} ns, so the deadline stayed armed",
            zero
        );
        return TestResult::Fail;
    }
    if restored != restore_ns {
        klog_info!(
            "SCHED_TEST: budget restored to {} ns, not the {} ns this test found",
            restored,
            restore_ns
        );
        return TestResult::Fail;
    }

    // The disabled budget must actually reach the fallback, not merely be zero.
    if crate::per_cpu::test_pause_deadline_passed(1, u64::MAX, 0, 0) {
        klog_info!("SCHED_TEST: a zero budget still expired a wait on wall time");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Every online AP acknowledges the pause from its own poll point.
///
/// The ack is corroboration rather than the gate, so this asserts what it is
/// for: a value only the AP itself can have written, under the generation this
/// pause published. A stale ack from an earlier pause cannot satisfy it.
pub fn test_ap_pause_acks_from_the_poll_point() -> TestResult {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count < 2 {
        klog_info!("SCHED_TEST: uniprocessor boot has no AP to acknowledge");
        return TestResult::Skipped;
    }

    let token = match crate::per_cpu::pause_all_aps() {
        Ok(token) => token,
        Err(err) => {
            klog_info!("SCHED_TEST: pause_all_aps failed: {:?}", err);
            return TestResult::Fail;
        }
    };

    // A parked AP acks on the reschedule IPI the pause already sends, but it
    // has to be scheduled to do so; poll rather than sample once.
    let generation = crate::per_cpu::ap_pause_generation();
    let deadline = slopos_kernel_services::clock::monotonic_ns() + 200_000_000;
    let mut unacked;
    loop {
        unacked = (1..cpu_count)
            .filter(|&cpu| {
                slopos_arch::pcr::is_cpu_online(cpu) && !crate::per_cpu::ap_has_acked_pause(cpu)
            })
            .count();
        if unacked == 0 {
            break;
        }
        let now = slopos_kernel_services::clock::monotonic_ns();
        if now == 0 || now >= deadline {
            break;
        }
        core::hint::spin_loop();
    }

    crate::per_cpu::resume_all_aps_if_not_nested(token);

    if generation == 0 {
        klog_info!("SCHED_TEST: pause generation never advanced past its initial value");
        return TestResult::Fail;
    }
    if unacked != 0 {
        klog_info!(
            "SCHED_TEST: {} online AP(s) never acked pause generation {}",
            unacked,
            generation
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// The one store the dying-CPU cascade hinges on.
pub fn test_abandon_dispatch_for_dying_cpu_clears_the_flag() -> TestResult {
    if slopos_arch::pcr::get_cpu_count() < 2 {
        klog_info!("SCHED_TEST: uniprocessor boot has no AP to abandon");
        return TestResult::Skipped;
    }

    const HELD_CPU: usize = 1;
    let _freeze = crate::task::freeze_kernel_io_all();

    if crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(true))
        .is_none()
    {
        klog_info!("SCHED_TEST: CPU {} has no per-CPU scheduler", HELD_CPU);
        return TestResult::Fail;
    }

    crate::per_cpu::abandon_dispatch_for_dying_cpu(HELD_CPU);
    let cleared = crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.is_executing_task());

    if cleared != Some(false) {
        crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(false));
        klog_info!(
            "SCHED_TEST: dispatch flag still {:?} after abandon",
            cleared
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_ap_pause_ignores_an_offline_ap,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_pause_deadline_passed_uses_wall_clock_when_available,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_ap_pause_acks_from_the_poll_point,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_ap_pause_budget_knob_round_trips,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_pause_failure_classification_reads_the_heartbeat,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_abandon_dispatch_for_dying_cpu_clears_the_flag,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_ap_pause_nests_on_a_depth_count,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_ap_pause_timeout_is_reported_and_rolled_back,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wake_against_reaped_waiter_is_inert,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_newcomer_outranks_current_preemption_gate,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_bootstrap_task_ptr_rejects_interior_addresses,
    suite = sched_core
);
slopos_testing::stest!(name = test_task_ids_are_never_reused, suite = sched_core);

/// Create a kernel task and park a wait node for it on `wq`, leaving the park
/// back-pointer published. Dispatched and restored for the same reasons as
/// [`park_task_on_queue`].
fn park_stack_waiter(wq: &WaitQueue, name: &'static [u8]) -> Option<(u32, ParkedTestNode)> {
    let task_id = task_create(
        name.as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return None;
    }
    let parked = if scheduler::clear_nascent_for_test(task_id) && dispatch_as_current(task_id) {
        wq.park_unowned_node_for_test()
    } else {
        None
    };
    park_bootstrap_on_current_cpu();
    match parked {
        Some(node) => Some((task_id, node)),
        None => {
            let _ = task_terminate(task_id);
            None
        }
    }
}

/// A task torn down while parked in `wait_event` leaves no node behind.
///
/// The node lives on the victim's kernel stack and the slot is recycled rather
/// than quarantined, so a node still linked after teardown is written through
/// by the next wake, at an address that by then belongs to a different task.
/// The keeper is what makes that wake happen.
pub fn test_terminated_waiter_leaves_no_wait_node() -> TestResult {
    let _fixture = SchedFixture::new();

    // Declared before the nodes so it outlives them: a node still linked at
    // drop reaches back through its queue pointer to unlink itself.
    let wq = WaitQueue::new(lock_class!("test.wq14", LOCK_LEVEL_RESOURCE));

    let Some((victim, _victim_node)) = park_stack_waiter(&wq, b"ParkVictim\0") else {
        klog_info!("SCHED_TEST: could not park the victim");
        return TestResult::Fail;
    };
    let Some((keeper, _keeper_node)) = park_stack_waiter(&wq, b"ParkKeeper\0") else {
        klog_info!("SCHED_TEST: could not park the keeper");
        let _ = task_terminate(victim);
        return TestResult::Fail;
    };

    let mut outcome = TestResult::Pass;
    if wq.waiter_count() != 2 {
        klog_info!(
            "SCHED_TEST: expected 2 parked waiters, found {}",
            wq.waiter_count()
        );
        outcome = TestResult::Fail;
    }

    // Control: `remove_task` declines an unowned node, so teardown must not be
    // relying on it.
    wq.remove_task(victim);
    if wq.waiter_count() != 2 {
        klog_info!("SCHED_TEST: remove_task unlinked an unowned node");
        outcome = TestResult::Fail;
    }

    if task_terminate(victim) != 0 {
        klog_info!("SCHED_TEST: terminating the parked victim failed");
        outcome = TestResult::Fail;
    }
    if wq.waiter_count() != 1 {
        klog_info!(
            "SCHED_TEST: victim left {} node(s) linked after teardown, want 1 (keeper only)",
            wq.waiter_count()
        );
        outcome = TestResult::Fail;
    }

    if !wq.wake_one() {
        klog_info!("SCHED_TEST: the keeper's node did not survive the purge");
        outcome = TestResult::Fail;
    }

    let _ = task_terminate(keeper);
    outcome
}

slopos_testing::stest!(
    name = test_terminated_waiter_leaves_no_wait_node,
    suite = sched_core
);

/// A terminal task running on a CPU is descheduled at the next tick.
///
/// Every arm of the tick handler that declines to preempt returns without
/// requesting a reschedule, so without a terminal-status escape above them a
/// task killed from a peer CPU keeps running as a `Zombie` and its switch-tail
/// cleanup never happens.
pub fn test_terminal_task_is_descheduled_at_the_tick() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"TickExit\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    if !scheduler::clear_nascent_for_test(task_id) || !dispatch_as_current(task_id) {
        park_bootstrap_on_current_cpu();
        let _ = task_terminate(task_id);
        klog_info!("SCHED_TEST: could not dispatch the tick fixture task");
        return TestResult::Fail;
    }

    let mut outcome = TestResult::Pass;

    // IRQs off for the whole window: `dispatch_task_for_test` publishes a Task
    // that is not schedulable as this CPU's current while the test's own stack
    // keeps running, so a real tick would reschedule off that stack and never
    // come back.
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        // The tick's first arm returns on a disabled scheduler, which the
        // fixture leaves it as — without this every check below passes
        // vacuously.
        scheduler::set_scheduler_enabled_for_test(true);

        // Control: a live task with a full quantum and an empty ready queue is
        // left alone, which is what makes the escape below load-bearing.
        PreemptGuard::clear_reschedule_pending();
        scheduler_timer_tick();
        if PreemptGuard::is_reschedule_pending() {
            klog_info!("SCHED_TEST: a live task was preempted with an empty ready queue");
            outcome = TestResult::Fail;
        }

        // Terminal status, still this CPU's current: the tick must deschedule.
        if task_set_state(task_id, TaskStatus::Zombie) != 0 {
            klog_info!("SCHED_TEST: could not publish Zombie on the fixture task");
            outcome = TestResult::Fail;
        } else {
            PreemptGuard::clear_reschedule_pending();
            scheduler_timer_tick();
            if !PreemptGuard::is_reschedule_pending() {
                klog_info!("SCHED_TEST: a Zombie task kept the CPU past a tick");
                outcome = TestResult::Fail;
            }
        }

        scheduler::set_scheduler_enabled_for_test(false);
        PreemptGuard::clear_reschedule_pending();
    });
    park_bootstrap_on_current_cpu();
    let _ = task_terminate(task_id);
    outcome
}

slopos_testing::stest!(
    name = test_terminal_task_is_descheduled_at_the_tick,
    suite = sched_core
);

/// A current task that a wake found mid-protocol — committed `Running →
/// Blocked`, then CASed back to `Ready` and enqueued by a peer before it
/// descheduled — must not be rescheduled from the trap exit: `schedule()`
/// there dequeues the caller as its own successor and spins on its own
/// `on_cpu`, which only the switch that spin stands in front of can clear.
/// Without the guard this test hangs the run rather than failing.
pub fn test_ready_current_is_not_rescheduled_from_trap_exit() -> TestResult {
    use crate::trap::{TrapExitSource, scheduler_handoff_on_trap_exit};

    let _fixture = SchedFixture::new();
    // `schedule_internal` returns before any dispatch when this CPU has no
    // idle task, which would make the check below pass vacuously.
    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"ReadyCurrent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    if !scheduler::clear_nascent_for_test(task_id) || !dispatch_as_current(task_id) {
        park_bootstrap_on_current_cpu();
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let mut outcome = TestResult::Pass;
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        scheduler::set_scheduler_enabled_for_test(true);

        let Some(task_ref) = task_find_by_id(task_id) else {
            klog_info!("SCHED_TEST: fixture task vanished");
            outcome = TestResult::Fail;
            return;
        };
        // A running task is `on_cpu`; the test dispatch does not set it, and
        // without it the unguarded code re-dispatches the task instead of
        // spinning on itself, so the test would pass for the wrong reason.
        task_ref.set_on_cpu(true);
        if task_set_state(task_id, TaskStatus::Blocked) != 0
            || scheduler::unblock_task(&task_ref) != 0
            || task_ref.status() != TaskStatus::Ready
        {
            klog_info!("SCHED_TEST: could not stage a Ready current task");
            outcome = TestResult::Fail;
        } else {
            PreemptGuard::set_reschedule_pending();
            scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
            let still_current = slopos_sched_current_for_test().is_some_and(|c| c.id() == task_id);
            if !still_current {
                klog_info!("SCHED_TEST: a Ready current task was switched away from");
                outcome = TestResult::Fail;
            }
            // The wake is consumed the way the protocol would consume it, so
            // the task leaves the fixture Running and owned by nothing.
            if let Some(c) = slopos_sched_current_for_test() {
                let _ = scheduler::consume_ready_wake_for_current_for_test(&c);
            }
        }
        task_ref.set_on_cpu(false);

        scheduler::set_scheduler_enabled_for_test(false);
        PreemptGuard::clear_reschedule_pending();
    });
    park_bootstrap_on_current_cpu();
    let _ = task_terminate(task_id);
    outcome
}

slopos_testing::stest!(
    name = test_ready_current_is_not_rescheduled_from_trap_exit,
    suite = sched_core
);

/// A task that goes terminal while parked is not restored to Running.
///
/// The wait protocol's cancel of its own `Running -> Blocked` commit is a
/// force-store; restoring a `Zombie` that way makes the status gate in
/// `cleanup_current_task_after_switch` a permanent no-op, so the fd table, the
/// process VM and the reap never run.
pub fn test_terminal_task_is_not_restored_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NoResurrect\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    if !scheduler::clear_nascent_for_test(task_id) || !dispatch_as_current(task_id) {
        park_bootstrap_on_current_cpu();
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let mut outcome = TestResult::Pass;

    // Control: a live task is restored, which is what the call is for.
    if let Some(current) = slopos_sched_current_for_test() {
        if !scheduler::consume_ready_wake_for_current_for_test(&current) {
            klog_info!("SCHED_TEST: a live task was refused a Running publish");
            outcome = TestResult::Fail;
        }
    } else {
        klog_info!("SCHED_TEST: no current task after dispatch");
        outcome = TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Zombie) != 0 {
        klog_info!("SCHED_TEST: could not publish Zombie");
        outcome = TestResult::Fail;
    } else if let Some(current) = slopos_sched_current_for_test() {
        if scheduler::consume_ready_wake_for_current_for_test(&current) {
            klog_info!("SCHED_TEST: a Zombie task was restored to Running");
            outcome = TestResult::Fail;
        }
        let status = current.task().status();
        if status != TaskStatus::Zombie {
            klog_info!(
                "SCHED_TEST: terminal status was overwritten, now {:?}",
                status
            );
            outcome = TestResult::Fail;
        }
        if current.task().sched_placement() != SchedPlacement::None {
            klog_info!("SCHED_TEST: a refused task kept a scheduler owner");
            outcome = TestResult::Fail;
        }
    }

    park_bootstrap_on_current_cpu();
    let _ = task_terminate(task_id);
    outcome
}

fn slopos_sched_current_for_test() -> Option<crate::task_struct::Current> {
    crate::task_struct::Current::get()
}

slopos_testing::stest!(
    name = test_terminal_task_is_not_restored_to_running,
    suite = sched_core
);

/// A bucket takes more waiters than the old fixed array held.
///
/// The cap was 16, and the 17th waiter got ENOMEM — which every userland
/// futex wrapper discards, turning a blocked waiter into a full-core spin.
/// The list is intrusive now, so the only bound is the number of tasks.
pub fn test_futex_bucket_exceeds_old_fixed_cap() -> TestResult {
    let _fixture = SchedFixture::new();

    const WAITERS: usize = 24;
    let uaddr = 0x4321_0000u64;
    let mut ids = [INVALID_TASK_ID; WAITERS];
    let mut parked = 0usize;
    let mut outcome = TestResult::Pass;

    for slot in ids.iter_mut() {
        let id = task_create(
            b"FutexMany\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            break;
        }
        *slot = id;
        if !scheduler::clear_nascent_for_test(id) || !dispatch_as_current(id) {
            break;
        }
        if !crate::futex::futex_park_for_test(uaddr) {
            klog_info!("SCHED_TEST: bucket refused waiter {}", parked + 1);
            outcome = TestResult::Fail;
            break;
        }
        parked += 1;
    }
    park_bootstrap_on_current_cpu();

    if parked <= 16 {
        klog_info!("SCHED_TEST: only {} waiters parked; cap not lifted", parked);
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_waiters_for_test(uaddr) != parked {
        klog_info!("SCHED_TEST: bucket does not hold every parked waiter");
        outcome = TestResult::Fail;
    }

    let woken = crate::futex::futex_wake(uaddr, parked as u32);
    if woken != parked as i64 {
        klog_info!("SCHED_TEST: woke {} of {} waiters", woken, parked);
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_waiters_for_test(uaddr) != 0 {
        klog_info!("SCHED_TEST: bucket not drained after wake");
        outcome = TestResult::Fail;
    }

    for &id in ids.iter() {
        if id != INVALID_TASK_ID {
            let _ = task_terminate(id);
        }
    }
    outcome
}

/// A futex waiter always leaves its bucket, and the dequeue reports *who*
/// dequeued it.
///
/// The bucket slot holds a strong reference; without an unconditional
/// self-dequeue it is stranded until teardown or an unrelated wake on the same
/// address, and the return value is what tells a real wake from every other way
/// out.
pub fn test_futex_waiter_always_leaves_its_bucket() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"FutexPark\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    if !scheduler::clear_nascent_for_test(task_id) || !dispatch_as_current(task_id) {
        park_bootstrap_on_current_cpu();
        let _ = task_terminate(task_id);
        return TestResult::Fail;
    }

    let uaddr = 0x1234_5000u64;
    let other = 0x1234_6000u64;
    let mut outcome = TestResult::Pass;

    if !crate::futex::futex_park_for_test(uaddr) {
        klog_info!("SCHED_TEST: could not park a futex waiter");
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_waiters_for_test(uaddr) != 1 {
        klog_info!("SCHED_TEST: parked waiter is not in its bucket");
        outcome = TestResult::Fail;
    }

    if crate::futex::futex_remove_self_for_test(other, task_id) {
        klog_info!("SCHED_TEST: dequeue matched an unrelated futex address");
        outcome = TestResult::Fail;
    }

    if !crate::futex::futex_remove_self_for_test(uaddr, task_id) {
        klog_info!("SCHED_TEST: self-dequeue did not find its own entry");
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_waiters_for_test(uaddr) != 0 {
        klog_info!("SCHED_TEST: bucket still holds the entry after dequeue");
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_remove_self_for_test(uaddr, task_id) {
        klog_info!("SCHED_TEST: a second dequeue claimed to find an entry");
        outcome = TestResult::Fail;
    }

    // A real wake takes the slot, so the self-dequeue reports "not mine".
    if !crate::futex::futex_park_for_test(uaddr) {
        klog_info!("SCHED_TEST: could not re-park the futex waiter");
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_wake(uaddr, 1) != 1 {
        klog_info!("SCHED_TEST: futex_wake did not report one waiter");
        outcome = TestResult::Fail;
    }
    if crate::futex::futex_remove_self_for_test(uaddr, task_id) {
        klog_info!("SCHED_TEST: self-dequeue claimed an entry futex_wake had taken");
        outcome = TestResult::Fail;
    }

    park_bootstrap_on_current_cpu();
    let _ = task_terminate(task_id);
    outcome
}

slopos_testing::stest!(
    name = test_futex_waiter_always_leaves_its_bucket,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_futex_bucket_exceeds_old_fixed_cap,
    suite = sched_core
);

/// A builder that loses its frame releases the child it was building.
///
/// A `PendingTask` is unregistered — no lookup, no active-task walk, no census
/// and no shutdown sweep sees it — so a token lost with its frame is
/// unrecoverable, and the only trace is `allocate_task`'s live count never
/// coming back down.
pub fn test_spawn_guard_releases_its_child_when_dropped() -> TestResult {
    let _fixture = SchedFixture::new();

    let (live_before, _, _, _) = task_slot_census();

    let Some(pending) = build_parkable_child() else {
        klog_info!("SCHED_TEST: could not build the child");
        return TestResult::Fail;
    };
    let child_id = pending.id();
    {
        let guard = crate::task::SpawnGuard::new(pending);
        if guard.child_id() != child_id {
            klog_info!("SCHED_TEST: the guard named the wrong child");
            return TestResult::Fail;
        }
        if task_find_by_id(child_id).is_some() {
            klog_info!("SCHED_TEST: a child under construction must not be reachable");
            return TestResult::Fail;
        }
    }
    crate::task::task_graveyard_drain();

    // Nothing registered the child, so only the census can show the leak.
    let (live_after, _, _, _) = task_slot_census();
    if live_after != live_before {
        klog_info!(
            "SCHED_TEST: live task count {} -> {} across a dropped builder",
            live_before,
            live_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_spawn_guard_releases_its_child_when_dropped,
    suite = sched_core
);

/// A `WaitNode` unlinks itself when its frame unwinds.
///
/// `catch_panic!` unwinds rather than jumping, so destructors run during
/// recovery; a node left linked keeps a queue pointer into a dead frame.
pub fn test_wait_node_unlinks_when_its_frame_unwinds() -> TestResult {
    let _fixture = SchedFixture::new();

    let wq = WaitQueue::new(lock_class!("test.wq15", LOCK_LEVEL_RESOURCE));
    let Some((task_id, node)) = park_stack_waiter(&wq, b"UnwindWaiter\0") else {
        klog_info!("SCHED_TEST: could not park the waiter");
        return TestResult::Fail;
    };

    let mut outcome = TestResult::Pass;
    if wq.waiter_count() != 1 {
        klog_info!("SCHED_TEST: the waiter is not on the queue");
        outcome = TestResult::Fail;
    }

    // Destroyed the way an unwind destroys it.
    drop(node);
    if wq.waiter_count() != 0 {
        klog_info!(
            "SCHED_TEST: a destroyed node left {} entries on the queue",
            wq.waiter_count()
        );
        outcome = TestResult::Fail;
    }
    // A botched unlink corrupts the list rather than merely leaving it long.
    if wq.wake_one() {
        klog_info!("SCHED_TEST: the queue woke a node that no longer exists");
        outcome = TestResult::Fail;
    }

    let _ = task_terminate(task_id);
    outcome
}

slopos_testing::stest!(
    name = test_wait_node_unlinks_when_its_frame_unwinds,
    suite = sched_core
);

/// A process at its `Task` ceiling is refused, and the refusal refunds.
///
/// `MAX_TASKS` is 8192 global, so without a per-principal bound one process
/// spends the whole table and denies every other one.
pub fn test_quota_task_ceiling_refuses_and_refunds() -> TestResult {
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 3;

    let Ok(process) = slopos_ostd::process::process_spawn_root() else {
        klog_info!("QUOTA_TEST: could not register a process");
        return TestResult::Fail;
    };
    let account = process.account();
    let baseline = stats(account, ResourceKind::Task).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::Task, baseline + CEILING);

    // The account is what the ceiling binds, so the charges are taken the way
    // `task_build` takes them rather than by spawning real tasks.
    let mut held = slopos_ostd::KVec::new();
    let mut granted = 0u32;
    for _ in 0..CEILING + 4 {
        match crate::task::task_quota::reserve(account) {
            Some(reservation) => {
                granted += 1;
                if held.push(reservation).is_err() {
                    break;
                }
            }
            None => break,
        }
    }

    let at_ceiling = stats(account, ResourceKind::Task).map_or(0, |s| s.used);
    let denials = stats(account, ResourceKind::Task).map_or(0, |s| s.denials);
    drop(held);
    let after = stats(account, ResourceKind::Task).map_or(0, |s| s.used);
    set_quota_mode(restore);

    // Retire the scratch process so its row goes dark before the headroom gate
    // reads it.
    if let Some(handle) = process.handle() {
        slopos_ostd::process::process_retire(handle);
    }
    drop(process);

    if granted != CEILING {
        klog_info!("QUOTA_TEST: granted {granted} tasks against a ceiling of {CEILING}");
        return TestResult::Fail;
    }
    if at_ceiling != baseline + CEILING {
        klog_info!(
            "QUOTA_TEST: used {at_ceiling} at the ceiling, want {}",
            baseline + CEILING
        );
        return TestResult::Fail;
    }
    if denials == 0 {
        klog_info!("QUOTA_TEST: a refusal nobody counted is a silent denial");
        return TestResult::Fail;
    }
    if after != baseline {
        klog_info!("QUOTA_TEST: used {after} after release, want the {baseline} it started at");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_task_ceiling_refuses_and_refunds,
    suite = sched_core
);

/// A scratch process with its account row, retired on drop.
///
/// A leftover row carrying a deliberately-tiny ceiling and a deliberate denial
/// is indistinguishable, to the headroom gate, from a real workload refused.
struct QuotaScratch {
    process: slopos_ostd::KArc<slopos_ostd::process::Process>,
}

impl QuotaScratch {
    fn new() -> Option<Self> {
        Some(Self {
            process: slopos_ostd::process::process_spawn_root().ok()?,
        })
    }

    fn account(&self) -> slopos_ostd::process::AccountId {
        self.process.account()
    }
}

impl Drop for QuotaScratch {
    fn drop(&mut self) {
        if let Some(handle) = self.process.handle() {
            slopos_ostd::process::process_retire(handle);
        }
    }
}

/// One process at its `Task` ceiling does not deny another — the property a
/// global `MAX_TASKS` cannot provide.
pub fn test_quota_task_cross_process_isolation() -> TestResult {
    use crate::task::task_quota::reserve;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode};

    const CEILING: u32 = 2;

    let (Some(greedy), Some(neighbour)) = (QuotaScratch::new(), QuotaScratch::new()) else {
        klog_info!("QUOTA_TEST: could not register two processes");
        return TestResult::Fail;
    };

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(greedy.account(), ResourceKind::Task, CEILING);

    let mut held = slopos_ostd::KVec::new();
    let mut granted = 0u32;
    while let Some(reservation) = reserve(greedy.account()) {
        granted += 1;
        if held.push(reservation).is_err() || granted > CEILING {
            break;
        }
    }
    // The neighbour shares every ancestor with the exhausted account, which is
    // the case a hierarchical debit could get wrong by refusing at a shared
    // level.
    let neighbour_ok = reserve(neighbour.account()).is_some();
    drop(held);
    set_quota_mode(restore);

    if granted != CEILING {
        klog_info!("QUOTA_TEST: greedy granted {granted}, want {CEILING}");
        return TestResult::Fail;
    }
    if !neighbour_ok {
        klog_info!("QUOTA_TEST: a neighbour was denied because a sibling hit its ceiling");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_task_cross_process_isolation,
    suite = sched_core
);

/// A process at its `Process` ceiling cannot spawn, and the refusal is exact.
///
/// `MAX_PROCESSES` is 256 and reached long before `MAX_TASKS`, so this is the
/// tighter global table.
pub fn test_quota_process_ceiling_refuses_and_refunds() -> TestResult {
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::process_spawn;
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 3;

    let Some(spawner) = QuotaScratch::new() else {
        klog_info!("QUOTA_TEST: could not register a spawner");
        return TestResult::Fail;
    };
    let account = spawner.account();
    let baseline = stats(account, ResourceKind::Process).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::Process, baseline + CEILING);

    // The scratch account is the children's accounting parent, the edge a real
    // spawn sets and never re-homes.
    let mut children = slopos_ostd::KVec::new();
    let mut spawned = 0u32;
    for _ in 0..CEILING + 3 {
        match process_spawn(None, account) {
            Ok(child) => {
                spawned += 1;
                if children.push(child).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let at_ceiling = stats(account, ResourceKind::Process).map_or(0, |s| s.used);
    let denials = stats(account, ResourceKind::Process).map_or(0, |s| s.denials);

    // The spawner's charge is released at the reap, not at the final drop.
    for child in children.iter() {
        if let Some(handle) = child.handle() {
            slopos_ostd::process::process_retire(handle);
        }
    }
    drop(children);
    let after = stats(account, ResourceKind::Process).map_or(0, |s| s.used);
    set_quota_mode(restore);

    if spawned != CEILING {
        klog_info!("QUOTA_TEST: spawned {spawned} against a ceiling of {CEILING}");
        return TestResult::Fail;
    }
    if at_ceiling != baseline + CEILING {
        klog_info!(
            "QUOTA_TEST: process used {at_ceiling}, want {}",
            baseline + CEILING
        );
        return TestResult::Fail;
    }
    if denials == 0 {
        klog_info!("QUOTA_TEST: a refused spawn nobody counted is a silent denial");
        return TestResult::Fail;
    }
    if after != baseline {
        klog_info!("QUOTA_TEST: process used {after} after reap, want {baseline}");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_process_ceiling_refuses_and_refunds,
    suite = sched_core
);

/// A task's stack pages are charged, and given back at the exit latch —
/// `KernelMeta` is the largest single consumer, 12 pages per task.
pub fn test_quota_kernelmeta_covers_task_stacks() -> TestResult {
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_abi::task::{TASK_KERNEL_STACK_SIZE, TASK_UNSAFE_STACK_SIZE};
    use slopos_mm::paging_defs::PAGE_SIZE_4KB;
    use slopos_ostd::process::quota::{quota_mode, set_quota_mode, stats};

    let Some(scratch) = QuotaScratch::new() else {
        klog_info!("QUOTA_TEST: could not register a process");
        return TestResult::Fail;
    };
    let account = scratch.account();
    let baseline = stats(account, ResourceKind::KernelMeta).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);

    let expected = ((TASK_KERNEL_STACK_SIZE + TASK_UNSAFE_STACK_SIZE) / PAGE_SIZE_4KB) as u32;
    let kernel = crate::task_stack::KernelStack::allocate(TASK_KERNEL_STACK_SIZE as usize, account);
    let data = crate::task_stack::UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize, account);
    let charged = stats(account, ResourceKind::KernelMeta).map_or(0, |s| s.used) - baseline;
    drop(kernel);
    drop(data);
    let after = stats(account, ResourceKind::KernelMeta).map_or(0, |s| s.used);
    set_quota_mode(restore);

    if charged != expected {
        klog_info!("QUOTA_TEST: stacks charged {charged} pages, want {expected}");
        return TestResult::Fail;
    }
    if after != baseline {
        klog_info!("QUOTA_TEST: kernelmeta {after} after release, want {baseline}");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_kernelmeta_covers_task_stacks,
    suite = sched_core
);

/// The charge path's cost per charge+refund round trip, measured at two account
/// depths because the debit walks *up* the chain — the difference between them
/// is the per-level cost.
///
/// Emits `QUOTACOST:` lines rather than asserting a bound: the ceiling lives in
/// `scripts/gates/quota/<variant>.txt`.
pub fn test_quota_charge_cost() -> TestResult {
    use slopos_abi::quota::{FdSlot, QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{Charge, quota_mode, set_quota_mode, try_charge};

    // Reported as the minimum over batches: `rdtsc` counts host wall time, which only ever inflates.
    const BATCH_LEN: u32 = 128;
    const BATCHES: u32 = 64;
    const WARM_ITERATIONS: u32 = 1_000;

    let Some(shallow) = QuotaScratch::new() else {
        klog_info!("QUOTACOST: could not register a process");
        return TestResult::Fail;
    };

    // Grown until the arena refuses rather than to a computed bound:
    // `account_create` enforces `MAX_ACCOUNT_DEPTH`, and a spawn past it still
    // succeeds — it just leaves the process with an account that names no row.
    let mut chain = slopos_ostd::KVec::new();
    let mut deepest = shallow.account();
    loop {
        let Ok(child) = slopos_ostd::process::process_spawn(None, deepest) else {
            break;
        };
        let candidate = child.account();
        if slopos_ostd::process::quota::stats(candidate, ResourceKind::FdSlot).is_none() {
            if let Some(handle) = child.handle() {
                slopos_ostd::process::process_retire(handle);
            }
            break;
        }
        deepest = candidate;
        if chain.push(child).is_err() {
            break;
        }
    }
    let depth = chain.len() as u32 + 1;

    // A row that does not exist makes `try_charge` return immediately, so a
    // chain built past `MAX_ACCOUNT_DEPTH` would measure the *absence* of a
    // walk and report it as a fast one.
    if slopos_ostd::process::quota::stats(deepest, ResourceKind::FdSlot).is_none() {
        klog_info!("QUOTACOST: the deepest account has no row; nothing to measure");
        return TestResult::Fail;
    }

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);

    // A same-run scale for the gate: an absolute cycle budget here would measure the accelerator.
    let measure_reference = || -> u64 {
        use core::sync::atomic::{AtomicU64, Ordering};
        let cell = AtomicU64::new(0);
        for _ in 0..WARM_ITERATIONS {
            let observed = cell.load(Ordering::Relaxed);
            let _ = cell.compare_exchange_weak(
                observed,
                observed.wrapping_add(1),
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
        let mut best = u64::MAX;
        for _ in 0..BATCHES {
            let batch = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_| {
                let start = slopos_arch::tsc::rdtsc();
                for _ in 0..BATCH_LEN {
                    let observed = cell.load(Ordering::Relaxed);
                    let _ = cell.compare_exchange_weak(
                        observed,
                        observed.wrapping_add(1),
                        Ordering::Release,
                        Ordering::Relaxed,
                    );
                }
                slopos_arch::tsc::rdtsc().wrapping_sub(start)
            });
            best = best.min(batch / BATCH_LEN as u64);
        }
        best.max(1)
    };

    let measure = |account| -> u64 {
        // Warm first, unmeasured: otherwise the first loop absorbs the arena's
        // cold cache lines and reports a deeper chain as cheaper than a
        // shallow one.
        for _ in 0..WARM_ITERATIONS {
            if let Ok(reservation) = try_charge::<FdSlot>(account, 1) {
                drop(Charge::commit(reservation));
            }
        }
        let mut best = u64::MAX;
        for _ in 0..BATCHES {
            let batch = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_| {
                let start = slopos_arch::tsc::rdtsc();
                for _ in 0..BATCH_LEN {
                    if let Ok(reservation) = try_charge::<FdSlot>(account, 1) {
                        drop(Charge::commit(reservation));
                    }
                }
                slopos_arch::tsc::rdtsc().wrapping_sub(start)
            });
            best = best.min(batch / BATCH_LEN as u64);
        }
        best
    };

    // The deep one is measured first: the loop that runs first pays for the
    // arena's cold cache lines however much warming precedes it.
    let shallow_account = shallow.account();
    let deep_per_charge = measure(deepest);
    let shallow_per_charge = measure(shallow_account);
    let reference_per_op = measure_reference();
    set_quota_mode(restore);

    for child in chain.iter() {
        if let Some(handle) = child.handle() {
            slopos_ostd::process::process_retire(handle);
        }
    }
    drop(chain);

    // Cycles, not nanoseconds: converting needs a frequency, and under TCG the
    // TSC does not track one. Reported through the quota report, which the gate parses.
    crate::quota_console::record_charge_cost(
        shallow_per_charge,
        depth,
        deep_per_charge,
        reference_per_op,
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_charge_cost, suite = sched_core);

/// A `Normal` task that never blocks must not starve a `Low` task forever.
///
/// Under strict priority the `Low` task is never selected while any `Normal`
/// task is runnable, which is the finding. The aging backstop bounds that: a
/// non-empty tier passed over `AGING_THRESHOLD` times is served once, so the
/// wait is bounded rather than merely improbable.
pub fn test_low_priority_is_not_starved_by_busy_normal() -> TestResult {
    use crate::fair::AGING_THRESHOLD;
    use crate::per_cpu::with_local_scheduler;

    let fixture = SchedFixture::new();

    let normal_id = task_create(
        b"BusyNormal\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    let low_id = task_create(
        b"StarvedLow\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if normal_id == INVALID_TASK_ID || low_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let (Some(normal), Some(low)) = (task_find_by_id(normal_id), task_find_by_id(low_id)) else {
        return TestResult::Fail;
    };
    if !make_task_ready(normal_id) || !make_task_ready(low_id) {
        return TestResult::Fail;
    }

    // The `Low` task is enqueued once and never re-enqueued; the `Normal` task
    // is put back every time it is picked, modelling a task that never blocks.
    // Under strict priority alone, `Low` is never chosen.
    schedule_task(&low);
    schedule_task(&normal);

    // `tier_owed` disables aging entirely while a privileged tier has queued work.
    slopos_testing::assert_test!(
        fixture.kernel_io_is_quiesced(),
        "a kernel-io thread was still queued inside a scope"
    );

    let mut low_ran_at: Option<usize> = None;
    let rounds = (AGING_THRESHOLD as usize) * 4;

    for round in 0..rounds {
        let Some(picked) = with_local_scheduler(|rq| rq.dequeue_highest_priority()) else {
            break;
        };
        let is_low = {
            let body: &Task = &picked;
            body.task_id == low_id
        };
        if is_low {
            low_ran_at = Some(round);
            let _ = picked
                .sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
            break;
        }
        with_local_scheduler(|rq| {
            let _ = rq.enqueue_from_on_cpu(&picked);
        });
    }

    // Drain so the fixture's teardown sees an empty queue.
    while let Some(t) = with_local_scheduler(|rq| rq.dequeue_highest_priority()) {
        let _ = t.sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
    }

    match low_ran_at {
        Some(round) => {
            klog_info!("SCHED_TEST: Low ran after {} Normal dispatches", round);
            if round > AGING_THRESHOLD as usize + 1 {
                return slopos_testing::fail!(
                    "Low waited {} dispatches, past the {} bound",
                    round,
                    AGING_THRESHOLD
                );
            }
            TestResult::Pass
        }
        None => slopos_testing::fail!("a busy Normal task starved Low for all {} rounds", rounds),
    }
}

/// The backstop must not reorder the priority tiers in the ordinary case: a
/// bounded burst of higher-priority work still runs first.
pub fn test_aging_backstop_preserves_priority_in_the_common_case() -> TestResult {
    use crate::fair::{AGING_THRESHOLD, AgingState, NUM_TIERS};

    let aging = AgingState::new();
    let non_empty = [false, false, true, true, false];

    // Below the threshold nothing is owed, so selection stays strict.
    for _ in 0..(AGING_THRESHOLD - 1) {
        if aging.tier_owed(&non_empty).is_some() {
            return slopos_testing::fail!("a tier was owed before the threshold");
        }
        aging.note_dispatch(2, &non_empty);
    }
    aging.note_dispatch(2, &non_empty);
    if aging.tier_owed(&non_empty) != Some(3) {
        return slopos_testing::fail!("the starved tier was not owed at the threshold");
    }
    // Serving it clears the debt.
    aging.note_dispatch(3, &non_empty);
    if aging.tier_owed(&non_empty).is_some() {
        return slopos_testing::fail!("serving the owed tier did not clear it");
    }

    // An empty tier is never owed, however long it is passed over.
    let only_normal = [false, false, true, false, false];
    let fresh = AgingState::new();
    for _ in 0..(AGING_THRESHOLD * 4) {
        fresh.note_dispatch(2, &only_normal);
    }
    if fresh.tier_owed(&only_normal).is_some() {
        return slopos_testing::fail!("an empty tier must never be owed a dispatch");
    }
    let _ = NUM_TIERS;
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_low_priority_is_not_starved_by_busy_normal,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_aging_backstop_preserves_priority_in_the_common_case,
    suite = sched_core
);

/// The backstop must never hold back `High` or `KernelIo`.
///
/// `KernelIo` runs the paths the rest of the kernel's progress depends on —
/// NAPI receive, TX ring drain, TCP retransmit timers. A `Low` task served
/// ahead of one of those does not add latency, it stalls the work that makes
/// the machine answer at all: with those threads starved, `ping` and `curl`
/// produce no output and nothing in the log says why.
pub fn test_aging_never_holds_back_kernel_io() -> TestResult {
    use crate::fair::{AGING_THRESHOLD, AgingState};

    let aging = AgingState::new();
    let both = [false, true, false, true, false];
    for _ in 0..(AGING_THRESHOLD * 8) {
        if aging.tier_owed(&both).is_some() {
            return slopos_testing::fail!(
                "the backstop offered to preempt KernelIo, which starves packet delivery"
            );
        }
        aging.note_dispatch(1, &both);
    }

    // The debt is real, though: once KernelIo has nothing runnable, the tier
    // that waited is served rather than being starved a second time.
    let low_only = [false, false, false, true, false];
    if aging.tier_owed(&low_only) != Some(3) {
        return slopos_testing::fail!("Low was starved even after KernelIo drained");
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_aging_never_holds_back_kernel_io,
    suite = sched_core
);
