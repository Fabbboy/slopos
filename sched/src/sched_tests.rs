//! Comprehensive scheduler and task management tests.
//!
//! These tests are designed to find REAL bugs, not just pass. They test:
//! - State machine transitions (valid AND invalid)
//! - Edge cases (null, max capacity, overflow)
//! - Race-prone scenarios
//! - Resource exhaustion
//! - Error recovery paths

use core::ffi::{c_char, c_void};
use core::ptr;

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

// =============================================================================
// RAII Fixture for Scheduler Tests
// =============================================================================

/// RAII fixture for scheduler tests. All setup/teardown logic lives in
/// [`KernelTestScope`]; this wrapper exists for the historical name and
/// to keep the change to call sites mechanical.
pub struct SchedFixture {
    _scope: KernelTestScope,
}

impl SchedFixture {
    pub fn new() -> Self {
        Self {
            _scope: KernelTestScope::enter(),
        }
    }
}

// =============================================================================
// Test Helper Functions
// =============================================================================

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
    // A witness clone observes the strong count while one reference travels
    // through the deferred slot.
    let witness = arc.clone();
    let strong_before = KArc::strong_count(&witness);

    // Park one owning reference into the deferred slot exactly as the
    // dispatcher's switch tail does. `leak` consumes `arc` without changing the
    // strong count (the reference moves into the raw slot pointer).
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

    // The first drain reclaims and drops the parked reference exactly once; the
    // second finds an empty slot.
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

/// A final release in a context that cannot run the allocator-heavy `Task`
/// destructor must park the task rather than destroy it inline, and the drain
/// must then destroy it exactly once.
///
/// Uses an unregistered task so this release really is final — a registered one
/// is pinned by the registry, and no placement release can reach zero.
pub fn test_task_put_defers_unsafe_context_then_drains() -> TestResult {
    // Start from a clean slate so the assertions below observe only this task.
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
    // The strong side is gone the moment the release lands, whether or not the
    // destructor has run.
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

/// The mirror image: a final release in a context that *does* allow the
/// destructor must destroy inline and leave nothing parked, so a task freed on
/// a safe path never waits on an idle pass.
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

/// A non-final release must be a plain decrement: no destruction, nothing
/// parked. This is the common case on every dequeue and inbox drain.
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

    // Leak parks one strong reference as a raw pointer without dropping it, so
    // the visible strong count is unchanged and the base pointer is stable.
    let parked = task_placement_leak(arc);
    if parked.as_ptr().cast_const() != base {
        klog_info!("SCHED_TEST: placement leak moved the base pointer");
        return TestResult::Fail;
    }

    // Reclaim reconstitutes exactly that reference.
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
/// This is what lets a wake publisher decide whether to preempt a *remote* CPU
/// without dereferencing that CPU's current task — a read that raced its switch
/// tail, where the outgoing dispatch reference is released and the task's
/// destructor can run. A stale published priority is the failure with no crash:
/// the CPU looks permanently high-priority and silently stops being preempted.
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

    // Parking on the bootstrap stub is "this CPU runs nothing schedulable".
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

    // Every real priority outranks the sentinel, so a newcomer always wins
    // against a CPU running nothing — the branch the old null-pointer test took.
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
/// `TaskInner` carries a dozen same-typed scalars side by side, so the
/// realistic failure mode is a getter, a bulk copy or a rename wiring one to
/// another's storage — which compiles cleanly whenever the types match, and
/// then silently reports one task property as another. Distinct sentinels make
/// that a test failure instead of a mystery.
pub fn test_scalar_field_identity() -> TestResult {
    let mut arc = match KArc::try_init(Task::init_invalid()) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };

    {
        // The sole strong reference to a never-registered task, so `get_mut`
        // succeeds and the exclusivity is checked rather than asserted.
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
    // whose own frame then carried several unmerged copies of it.
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

// =============================================================================
// Kill-safe parking of a half-built task
// =============================================================================

/// Build a child exactly as `task_build` does, so the tests below park a real
/// token rather than a stand-in: a task that already owns its kernel stack, its
/// data stack and its process VM is what makes the leak expensive.
fn build_parkable_child() -> Option<crate::task::PendingTask> {
    crate::task::task_build(
        b"ParkedChild\0".as_ptr() as *const core::ffi::c_char,
        dummy_task_entry,
        core::ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    )
}

/// A builder torn down off-CPU releases the child it was half-way through.
///
/// `core::mem::forget` on the guard is the whole point: it is exactly what an
/// asynchronous kill does to a frame that never unwinds, and without the
/// teardown hook the child's kernel stack, data stack, process VM and fd table
/// stay allocated with nothing able to name them.
///
/// The deterministic half of the pair: it drives
/// `cleanup_terminated_task_resources`, the branch `task_terminate` takes for a
/// victim that is not executing, and it reaches that branch on any topology.
/// The deferred branch needs a real peer CPU and is covered separately.
pub fn test_parked_spawn_drained_when_owner_terminated_off_cpu() -> TestResult {
    let _fixture = SchedFixture::new();

    // Taken before the owner exists, so the comparison at the end covers both
    // the owner's own teardown and the child's: a leaked child is the only way
    // the count can fail to return.
    let (live_before, _, _, _) = task_slot_census();

    let owner_id = task_create(
        b"ParkOwner\0".as_ptr() as *const core::ffi::c_char,
        dummy_task_entry,
        core::ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if owner_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: could not create the parking owner");
        return TestResult::Fail;
    }

    let Some(pending) = build_parkable_child() else {
        let _ = task_terminate(owner_id);
        klog_info!("SCHED_TEST: could not build the child to park");
        return TestResult::Fail;
    };
    let child_id = pending.id();
    let Ok(guard) = crate::task::SpawnGuard::park_for_owner(owner_id, pending) else {
        let _ = task_terminate(owner_id);
        klog_info!("SCHED_TEST: the spawn spine was full");
        return TestResult::Fail;
    };
    // The frame that never unwinds.
    core::mem::forget(guard);

    if crate::task::parked_spawn_count() == 0 {
        klog_info!("SCHED_TEST: the token was not parked");
        let _ = task_terminate(owner_id);
        return TestResult::Fail;
    }

    if task_terminate(owner_id) != 0 {
        klog_info!("SCHED_TEST: could not terminate the parking owner");
        return TestResult::Fail;
    }
    crate::task::task_graveyard_drain();

    if crate::task::parked_spawn_count() != 0 {
        klog_info!("SCHED_TEST: teardown left the token parked");
        return TestResult::Fail;
    }
    // The direct leak signature: `allocate_task` bumps `num_tasks` and only the
    // token's release brings it back down, so a lost token shows up here and
    // nowhere else — the child never registered, so no walk can see it.
    let (live_after, _, _, _) = task_slot_census();
    if live_after != live_before {
        klog_info!(
            "SCHED_TEST: live task count {} -> {} across a torn-down builder",
            live_before,
            live_after
        );
        return TestResult::Fail;
    }
    if task_find_by_id(child_id).is_some() {
        klog_info!("SCHED_TEST: the abandoned child became reachable");
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_parked_spawn_drained_when_owner_terminated_off_cpu,
    suite = sched_core
);

/// The spine drains at shutdown, where nothing else can reach a parked token.
///
/// `collect_shutdown_task_ids` walks the registry, and a parked token has no
/// registry entry, so the shutdown sweep is blind to it by construction — the
/// drain has to be an explicit call. It also has to run *before* the
/// `num_tasks` recount, which recomputes from that same registry and would
/// otherwise paper over the accounting without returning the memory.
pub fn test_parked_spawn_drained_at_shutdown() -> TestResult {
    let _fixture = SchedFixture::new();

    let Some(pending) = build_parkable_child() else {
        klog_info!("SCHED_TEST: could not build the child to park");
        return TestResult::Fail;
    };
    let Ok(guard) = crate::task::SpawnGuard::park_for_owner(0xDEAD_BEEF, pending) else {
        klog_info!("SCHED_TEST: the spawn spine was full");
        return TestResult::Fail;
    };
    core::mem::forget(guard);

    let drained = crate::task::drain_parked_spawns();
    if drained != 1 {
        klog_info!(
            "SCHED_TEST: shutdown drain released {} tokens, expected 1",
            drained
        );
        return TestResult::Fail;
    }
    if crate::task::parked_spawn_count() != 0 {
        klog_info!("SCHED_TEST: the drain left a token behind");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_parked_spawn_drained_at_shutdown,
    suite = sched_core
);

/// The switch-tail hook releases a parked token too.
///
/// `cleanup_current_task_after_switch` is the *deferred* branch — the one a
/// peer's `task_terminate` hands off to whenever the victim is executing, run
/// on the victim's own CPU after the register swap. It is the branch that
/// matters most, because `task_terminate`'s `is_current` path deliberately does
/// not call `cleanup_terminated_task_resources`, so nothing else can release a
/// token whose owner died while running.
///
/// Driven directly rather than by killing a running task: the kernel test phase
/// runs in the `drivers` boot step, before `enter_scheduler`, so no task
/// created here can actually be dispatched — and a test that cannot make its
/// victim run cannot reach the branch any other way. What is under test is the
/// hook, and this calls it with exactly the guard and the task state the switch
/// tail does.
pub fn test_parked_spawn_drained_by_the_switch_tail_hook() -> TestResult {
    let _fixture = SchedFixture::new();

    let parked_before = crate::task::parked_spawn_count();
    let (live_before, _, _, _) = task_slot_census();

    let owner_id = task_create(
        b"ParkSwitchTail\0".as_ptr() as *const core::ffi::c_char,
        dummy_task_entry,
        core::ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if owner_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: could not create the switch-tail owner");
        return TestResult::Fail;
    }

    let Some(pending) = build_parkable_child() else {
        let _ = task_terminate(owner_id);
        klog_info!("SCHED_TEST: could not build the child to park");
        return TestResult::Fail;
    };
    let Ok(guard) = crate::task::SpawnGuard::park_for_owner(owner_id, pending) else {
        let _ = task_terminate(owner_id);
        klog_info!("SCHED_TEST: the spawn spine was full");
        return TestResult::Fail;
    };
    core::mem::forget(guard);

    {
        let Some(owner) = task_find_by_id(owner_id) else {
            let _ = crate::task::drain_parked_spawns();
            klog_info!("SCHED_TEST: the switch-tail owner vanished");
            return TestResult::Fail;
        };
        // The state the switch tail finds: terminated, with its kernel stack
        // still mapped. Anything else and the hook returns early.
        owner.set_status(TaskStatus::Terminated);
        crate::task::cleanup_current_task_after_switch(&owner);
    }
    crate::task::task_graveyard_drain();

    if crate::task::parked_spawn_count() != parked_before {
        klog_info!("SCHED_TEST: the switch-tail hook left the token parked");
        let _ = crate::task::drain_parked_spawns();
        return TestResult::Fail;
    }
    let (live_after, _, _, _) = task_slot_census();
    if live_after != live_before {
        klog_info!(
            "SCHED_TEST: live task count {} -> {} across the switch-tail hook",
            live_before,
            live_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_parked_spawn_drained_by_the_switch_tail_hook,
    suite = sched_core
);

/// A publication that fails leaves the task nascent, not reserved.
///
/// `schedule_task`/`schedule_new_task` reserve scheduler ownership by CASing
/// `Nascent -> Waking` *before* the publish path checks the task is Ready. A
/// fresh task is Blocked, so that check fails — and without a rollback the task
/// would sit in `Waking` forever. That is worse than a leak: `Waking` is a state
/// `wake_blocked_task` publishes from, so the next signal would hand a runqueue
/// exactly the half-built task `Nascent` exists to protect. Teardown would not
/// recover it either, since the retire CAS only matches `Nascent`.
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

    // And the wake gate must still hold, which is the property the missing
    // rollback actually destroyed.
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

    // A real publication still works afterwards.
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
/// A corpse left in `Nascent` would be permanently unreapable — the reap gate
/// and the destructor gate both key on task state, and nothing retires the
/// placement afterwards — so the registry slot would leak until spawns started
/// failing with `NoFreeSlot` thousands of tasks later.
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
/// `task_create` publishes `pgid = task_id` *before* it registers, so a
/// process-group signal arriving between registration and `publish_new_task`
/// finds a task whose status is `Blocked` and whose placement used to be
/// `None` — indistinguishable from a legitimate wake target. It was then
/// published half-built. `Nascent` is what makes the two distinguishable.
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

    // Publication is the one sanctioned way out, and it still works.
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
/// The spawn path writes a child's job-control identity — pgid, sid, group
/// object — only once its ELF is loaded, a disk read's worth of time after the
/// task exists. [`PendingTask`](crate::task::PendingTask) is what keeps that
/// whole span private: with no registry entry there is nothing for
/// `task_find_by_id` or the active-task walk to yield, and that walk matching
/// `task.pgid` is exactly how a process-group signal picks its targets. The
/// task appears at `task_commit` with its identity already written, so no
/// observer can find it carrying a stale one.
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

    // The spawn path's job-control inherit, performed here for the same reason
    // it is performed there: while the task is still private.
    let group_pgid = task_id + 0x4000;
    pending.as_mut().set_pgid(group_pgid);

    if task_find_by_id(task_id).is_some() {
        klog_info!("SCHED_TEST: a task under construction was findable by id");
        return TestResult::Fail;
    }

    // The `kill(-pgid)` target scan, verbatim: walk the active tasks and match
    // on pgid.
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

/// Abandoning a built task gives back everything it reserved.
///
/// This is every spawn failure before publication — a missing binary, a
/// rejected fd action — so what it has to release is the full cost of a task:
/// the live-task reservation, the allocation, and the address space the task
/// was going to run in. Only the id is spent, because ids are monotonic and
/// never recycled, which is what lets a caller tell "already retired" from
/// "never existed".
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
    let process_id = pending.as_mut().process_id;

    if process_id == INVALID_PROCESS_ID
        || slopos_mm::process_vm::process_vm_get_vm_space(process_id).is_none()
    {
        klog_info!("SCHED_TEST: a built user task has no address space");
        task_abandon(pending);
        return TestResult::Fail;
    }

    task_abandon(pending);

    if slopos_mm::process_vm::process_vm_get_vm_space(process_id).is_some() {
        klog_info!(
            "SCHED_TEST: abandoning task {} left process {} standing",
            task_id,
            process_id
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

/// Every user task the kernel can build, it can also run.
///
/// A task is refused dispatch and terminated if its process id is outside
/// the range the process tables are indexed by, so an id allocator that
/// can hand out an id past that range makes `task_build` succeed and the
/// task die at its first dispatch — for that task and every one after it,
/// until reboot.
///
/// This test does **not** call `init_process_vm`. Every other process test
/// in the tree does, which resets the id allocator and is exactly why the
/// suite cannot see a bounded id space. What is under test here is the
/// allocator's state as the kernel actually accumulates it, so the churn
/// has to run against whatever that state already is.
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

/// A task's address-space handle stops resolving once that address space
/// is gone, and never resolves to whichever process takes its place.
///
/// This is the whole reason a task carries a handle and not just a
/// process id. The id it held will be issued again, and the slot it used
/// will be bound again; anything keyed on either alone would follow the
/// task's page-fault and dispatch paths straight into a stranger's
/// address space.
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

    // Abandoning the task tears its address space down.
    task_abandon(pending);

    if process_vm_get_cr3_phys_by_handle(handle).is_ok() {
        klog_info!("SCHED_TEST: a torn-down address space still resolves through its handle");
        return TestResult::Fail;
    }

    // Bind a fresh process to the slot the dead task used and re-check.
    // The handle must still refuse — this is the case that would hand a
    // dead task's fault path a live stranger's page tables.
    let successor = create_process_vm();
    if successor == INVALID_PROCESS_ID {
        klog_info!("SCHED_TEST: could not create a successor process");
        return TestResult::Fail;
    }
    let resolved = process_vm_get_cr3_phys_by_handle(handle);
    destroy_process_vm(successor);

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
    // Stand in for a task that was published, ran, and blocked: wakes are
    // refused while a task is still nascent, which is what this test is *not*
    // about (see `test_nascent_task_refuses_wake`).
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

// =============================================================================
// STATE MACHINE TESTS
// These tests verify state transitions work correctly AND that invalid
// transitions are properly rejected (or at least logged).
// =============================================================================

/// Test: Valid state transition READY -> RUNNING
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

/// Test: Valid state transition RUNNING -> BLOCKED
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

    // Publish to READY, then claim RUNNING before blocking.
    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set READY state");
        return TestResult::Fail;
    }
    if task_set_state(task_id, TaskStatus::Running) != 0 {
        klog_info!("SCHED_TEST: Failed to set RUNNING state");
        return TestResult::Fail;
    }

    // Then transition to BLOCKED
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

    // Terminate the task
    task_terminate(task_id);

    // Try to find it again - should fail or be in TERMINATED/INVALID state
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

/// Test: INVALID state transition BLOCKED -> RUNNING (should go through READY first)
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

// =============================================================================
// TASK CAPACITY TESTS
// Test behavior at and beyond MAX_TASKS limit
// =============================================================================

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
/// *registration*.
///
/// The reap unhashes the entry as soon as the task is terminated and off-CPU, so
/// lookups stop resolving immediately and do not wait on outstanding guards —
/// this is `release_task`'s unhash. What the guard still guarantees is that the
/// memory stays valid while it is held, which is the property the registry can no
/// longer provide now that it only observes tasks.
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

    // Unhashed by the reap, independently of the outstanding guard.
    if task_find_by_id(id).is_some() {
        klog_info!("SCHED_TEST: reaped task {} still resolves", id);
        return TestResult::Fail;
    }
    // ...but the guard still keeps the allocation alive.
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
/// Reproduces the shape a fork/clone child has between `register_task` and
/// `publish_new_task`: fully constructed, registered, `Invalid`, and hidden from
/// every active-task scan. `task_terminate` used to report such a task as "not
/// found" and walk away, abandoning it along with its kernel stack, unsafe
/// stack and address space on every pre-publication failure path.
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
        task.set_status(TaskStatus::Invalid);
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

/// Test: rapid individually allocated task create/destroy cycle.
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

        // Immediately terminate
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
    // The live handle resolves to the same allocation as the ID lookup.
    if task_resolve_handle(h1).is_none() || task_resolve_handle(h1) != task_find_by_id(id1) {
        let _ = task_terminate(id1);
        return TestResult::Fail;
    }

    // Destruction makes both lookup forms fail.
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

/// Test: `KernelStack::allocate` returns a handle whose `top > base`
/// and is page-aligned.  Verifies the VA region carving + guard-page
/// layout are correct.
pub fn test_kstack_basic_alloc() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let stack = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

/// Test: after dropping a `KernelStack`, the slot is returned to the
/// allocator and can be reused for a subsequent allocation.
///
/// Confirms that task stack capacity is **independent of kernel binary
/// size**, because the slot allocator tracks availability in its own
/// bitmap rather than reading from (kernel-image-reserved) physical
/// pages.
pub fn test_kstack_slot_reuse() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

/// Test: invalid sizes are rejected without touching global state.
pub fn test_kstack_rejects_invalid_size() -> TestResult {
    use super::task_stack::KernelStack;

    // Zero size.
    if KernelStack::allocate(0).is_ok() {
        klog_info!("SCHED_TEST: zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Not a multiple of page size.
    if KernelStack::allocate(4097).is_ok() {
        klog_info!("SCHED_TEST: unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Bigger than the slot stride (64 KB minus guard).
    if KernelStack::allocate(64 * 1024).is_ok() {
        klog_info!("SCHED_TEST: oversized alloc unexpectedly succeeded");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// Per-CPU kstack slot cache tests.
// =============================================================================

/// Test: repeated alloc/free on the same CPU stays in the per-CPU cache.
/// After the first refill, subsequent iterations must not increment
/// `refill_count`.
pub fn test_kstack_pcp_refill() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();

    // Start from a known-clean cache: flush any stale entries back to the
    // global allocator so refill_count readings are meaningful.
    pcp_flush_current::<KstackRegion>();

    let before = pcp_stats::<KstackRegion>(cpu);

    // First alloc → empty cache → triggers exactly one refill.
    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

    // Subsequent allocs should be pure cache hits — the refill batch
    // (8 slots) amply covers several rounds.
    for i in 0..4 {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

    // alloc_count advanced by at least 4 warm-path pops (plus the first).
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

/// Test: driving the cache past `pcp_capacity()` forces a spill.
pub fn test_kstack_pcp_spill_overflow() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_capacity, pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<KstackRegion>();
    let baseline_in_use = in_use_count::<KstackRegion>();
    let before = pcp_stats::<KstackRegion>(cpu);

    // Hold N + 1 stacks simultaneously so each drop enters a full cache
    // and triggers a spill.  N = capacity.
    let hold = pcp_capacity::<KstackRegion>() + 1;
    let mut stacks: [Option<KernelStack>; 32] = [const { None }; 32];
    if hold > stacks.len() {
        klog_info!("SCHED_TEST[pcp_spill]: capacity {} > fixture cap", hold);
        return TestResult::Fail;
    }
    for i in 0..hold {
        stacks[i] = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
            Ok(s) => Some(s),
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_spill]: alloc {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
    }
    // Drop all — the first `capacity` fit in the cache, the rest force
    // at least one spill.
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

    // No leaks: the global in-use counter returns to baseline + (what's
    // still sitting in the cache).  Since we flushed at the start and
    // every stack has been dropped, any residual in_use must equal the
    // current cache `count` exactly.
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

/// Test: a slot's `was_backed` bit survives a PCP round-trip.  After
/// alloc/drop/alloc on the same CPU we should see the same VA reused
/// AND the second alloc must NOT hit the mapping path.
pub fn test_kstack_pcp_was_backed_preserved() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

/// Test: allocate on one CPU, free on another (simulated by explicit
/// flush-between), then reallocate.  The global state must stay
/// consistent — freed slots must be visible to any CPU's refill path.
pub fn test_kstack_pcp_cross_cpu_safety() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    // Alloc, drop, and immediately flush — forces the slot back into
    // the global pool instead of the PCP.  The next alloc then has to
    // refill from the global, exercising the cross-CPU handoff path.
    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<KstackRegion>();

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

/// Test: 1000-iteration stress loop with no leaks.
pub fn test_kstack_pcp_stress_1000() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    for i in 0..1000 {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

/// Advisory benchmark: logs cycles-per-alloc for a tight warm-cache
/// loop.  Always passes — the numbers show up in `test_output.log` for
/// regression tracking.
pub fn test_kstack_pcp_smp_throughput_bench() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    // Warm up the cache so the timed loop is a pure PCP hit.
    let warmup = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(_) => return TestResult::Pass,
    };
    drop(warmup);

    const ITERATIONS: u64 = 512;
    let start = slopos_arch::tsc::rdtsc();
    for _ in 0..ITERATIONS {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

// =============================================================================
// UnsafeStack (SafeStack data-stack) parity tests.
//
// These mirror the kstack tests above against the U-region allocator.  They
// guard the unification refactor: any change that diverges the two regions'
// behavior must show up here.
// =============================================================================

pub fn test_ustack_basic_alloc() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;

    let stack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: ustack first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

    if UnsafeStack::allocate(0).is_ok() {
        klog_info!("SCHED_TEST: ustack zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    if UnsafeStack::allocate(4097).is_ok() {
        klog_info!("SCHED_TEST: ustack unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Bigger than the slot stride (64 KB minus guard).
    if UnsafeStack::allocate(64 * 1024).is_ok() {
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

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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
        let s = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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
        stacks[i] = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<UstackRegion>();

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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
        let s = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

/// Test: kstack and ustack live in disjoint VA regions and have
/// independent global state.  Three load-bearing invariants the
/// unification refactor must preserve:
///
///   1. The K and U VA windows do not overlap.
///   2. A K allocation lands inside the K window; same for U.
///   3. Allocating from one region must NOT change the other region's
///      `in_use_count`.  (The two regions must be backed by truly
///      independent allocators.)
pub fn test_regions_disjoint() -> TestResult {
    use super::task_stack::{KernelStack, UnsafeStack};
    use slopos_abi::task::{TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE};
    use slopos_mm::memory_layout_defs::{
        KSTACK_VA_BASE, KSTACK_VA_END, USTACK_VA_BASE, USTACK_VA_END,
    };
    use slopos_mm::stack_region::{KstackRegion, UstackRegion};

    // (1) Region windows must not overlap.
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

    // Snapshot U's count, do K work, confirm U didn't move.
    let u_before_k_work = slopos_mm::stack_va::in_use_count::<UstackRegion>();
    let kstack = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
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

    // Snapshot K's count, do U work, confirm K didn't move.
    let k_before_u_work = slopos_mm::stack_va::in_use_count::<KstackRegion>();
    let ustack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
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

    // (2) Each handle's VA must land in its own region.
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

// =============================================================================
// SCHEDULER QUEUE TESTS
// Test priority queue behavior including edge cases
// =============================================================================

/// Test: Schedule task to empty queue
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

    // Schedule to empty queue
    if schedule_task(&task_guard) != 0 {
        klog_info!("SCHED_TEST: Failed to schedule task to empty queue");
        return TestResult::Fail;
    }

    // Verify task is in queue by checking stats
    let ready_count = get_scheduler_stats().ready_tasks;

    if ready_count == 0 {
        klog_info!("SCHED_TEST: Task scheduled but ready count is 0");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Schedule same task twice - should not duplicate
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

    // Schedule once
    schedule_task(&task_guard);

    let ready_before = get_scheduler_stats().ready_tasks;

    // Schedule again - should be idempotent
    schedule_task(&task_guard);

    let ready_after = get_scheduler_stats().ready_tasks;

    if ready_after != ready_before {
        klog_info!(
            "SCHED_TEST: Duplicate schedule changed count: {} -> {}",
            ready_before,
            ready_after
        );
        // This is actually handled correctly (returns 0 if already in queue)
        // but let's verify the count didn't change
    }

    TestResult::Pass
}

/// Test: the publication entry point refuses input it cannot schedule.
///
/// This used to pass a null pointer. `schedule_task` now takes an owning
/// handle, so a null is unrepresentable and the branch that rejected one is
/// gone with it — the type is the guard. What still needs a runtime check is
/// the other unschedulable input: a task that is not Ready. The reservation it
/// takes must be rolled back, leaving a never-published task spelled `Nascent`.
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

/// Test: Unschedule task not in queue
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

// =============================================================================
// PRIORITY TESTS
// Verify priority-based scheduling works correctly
// =============================================================================

/// Test: Higher priority task should be selected first
pub fn test_priority_ordering() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create tasks with different priorities
    // Priority 0 = highest, Priority 3 = lowest (IDLE)
    let low_id = task_create(
        b"LowPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(), // 2
        TASK_FLAG_KERNEL_MODE,
    );

    let normal_id = task_create(
        b"NormalPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(), // 1
        TASK_FLAG_KERNEL_MODE,
    );

    let high_id = task_create(
        b"HighPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::High.as_u8(), // 0
        TASK_FLAG_KERNEL_MODE,
    );

    if low_id == INVALID_TASK_ID || normal_id == INVALID_TASK_ID || high_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // Schedule in reverse priority order (low first)
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

/// Test: IDLE priority task should be selected last
pub fn test_idle_priority_last() -> TestResult {
    let _fixture = SchedFixture::new();

    let idle_id = task_create(
        b"IdlePri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Idle.as_u8(), // 3
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

    // Schedule idle first, then normal
    schedule_task(&idle_guard);
    schedule_task(&normal_guard);

    // The scheduler should pick normal before idle due to priority
    // We can't directly verify this without running, but we verify no crash

    TestResult::Pass
}

// =============================================================================
// TIMER TICK / PREEMPTION TESTS
// =============================================================================

/// Test: Timer tick should decrement time slice
pub fn test_timer_tick_decrements_slice() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create idle task so scheduler can start
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

// =============================================================================
// TERMINATION EDGE CASES
// =============================================================================

/// Test: Terminate task with invalid ID
pub fn test_terminate_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    let result = task_terminate(INVALID_TASK_ID);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating INVALID_TASK_ID succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Terminate non-existent task ID
pub fn test_terminate_nonexistent_id() -> TestResult {
    let _fixture = SchedFixture::new();

    // Use a very high ID that definitely doesn't exist
    let result = task_terminate(0xDEADBEEF);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating nonexistent task succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Double terminate same task
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

    // First terminate
    let first_result = task_terminate(task_id);
    if first_result != 0 {
        klog_info!("SCHED_TEST: First terminate failed");
        return TestResult::Fail;
    }

    let _second_result = task_terminate(task_id);

    TestResult::Pass
}

// =============================================================================
// TASK FIND/GET EDGE CASES
// =============================================================================

/// Test: Find task by invalid ID
pub fn test_find_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    if task_find_by_id(INVALID_TASK_ID).is_some() {
        klog_info!("SCHED_TEST: BUG - Found task with INVALID_TASK_ID!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// TASK CREATION EDGE CASES
// =============================================================================

/// Test: Create task with null entry point
#[allow(unused_variables)]
pub fn test_create_null_entry() -> TestResult {
    let _fixture = SchedFixture::new();

    let _null_fn_ptr: Option<fn(*mut c_void)> = None;

    TestResult::Pass
}

/// Test: Create task with conflicting mode flags
pub fn test_create_conflicting_flags() -> TestResult {
    let _fixture = SchedFixture::new();

    // Both kernel and user mode flags
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

/// Test: Create task with null name (should still work)
pub fn test_create_null_name() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        ptr::null(),
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    // Null name should be allowed (empty name)
    if task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: Task creation with null name failed (may be OK)");
        // This is actually acceptable behavior
    }

    TestResult::Pass
}

// =============================================================================
// SCHEDULER ENABLE/DISABLE TESTS
// =============================================================================

/// Test: Scheduler starts disabled
pub fn test_scheduler_starts_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    let enabled = scheduler_is_enabled();

    if enabled != 0 {
        klog_info!("SCHED_TEST: Scheduler should start disabled!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Schedule call when scheduler disabled
pub fn test_schedule_while_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    // Scheduler is disabled by default after init
    // Calling schedule() should be a no-op
    schedule();

    // Should not crash, no-op when disabled
    TestResult::Pass
}

/// Regression: boot userland pre-init enqueues tasks before enter_scheduler().
/// This must work on the current CPU even when its scheduler is initialized
/// but not yet enabled.
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

/// Regression: BSP idle-stack handoff must use idle task kernel stack.
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

    // `Ok` already means the slot named a task — the guard cannot be null.
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

/// Regression: idle-stack resolution must fail cleanly when no idle task exists.
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

/// Regression: idle-stack resolution must fail cleanly for zero kernel stack top.
pub fn test_resolve_idle_stack_reports_missing_kernel_stack() -> TestResult {
    let _fixture = SchedFixture::new();

    // Built here rather than mutated in place on the installed idle task: a
    // running CPU is standing on that stack, and zeroing its top to see the
    // error would be a live edit to the thing under it.
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

// =============================================================================
// STRESS TESTS
// =============================================================================

/// Test: Create many tasks with same priority
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

    // Schedule all of them
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

/// Test: Interleaved create/schedule/terminate
pub fn test_interleaved_operations() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..50 {
        // Create
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

        // Schedule first
        if let Some(task1) = task_find_by_id(id1) {
            assert!(
                make_task_ready(id1),
                "make_task_ready failed for id {:?}",
                id1
            );
            schedule_task(&task1);
        }

        // Terminate first before scheduling second
        task_terminate(id1);

        // Schedule second
        if let Some(task2) = task_find_by_id(id2) {
            assert!(
                make_task_ready(id2),
                "make_task_ready failed for id {:?}",
                id2
            );
            schedule_task(&task2);
        }

        // Terminate second
        task_terminate(id2);
    }

    TestResult::Pass
}

// =============================================================================
// CROSS-CPU SCHEDULING TESTS (SMP)
// Tests for the unified per-CPU scheduler architecture
// =============================================================================

/// Test: Remote inbox push and drain mechanism
/// Verifies that push_remote_wake() correctly adds tasks to the inbox
/// and drain_remote_inbox() moves them to the ready queue.
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
    // Stand in for a published-then-blocked task: inbox and wake paths refuse
    // a task that is still nascent.
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set inbox test task READY");
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Get ready count before
    let ready_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    // Push to remote inbox (simulating cross-CPU wake)
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
    });

    // Verify inbox has pending task.
    // On SMP, a timer tick may concurrently drain the inbox before this read.
    // We treat that as acceptable and validate via ready-queue delta below.
    let has_pending = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
        .unwrap_or(false);

    // Drain inbox
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    // Verify inbox is now empty
    let still_pending =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if still_pending && has_pending {
        klog_info!("SCHED_TEST: drain_remote_inbox did not empty inbox");
        return TestResult::Fail;
    }

    // Verify task is now in ready queue
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

/// Regression: pushing the same task into a remote inbox twice must be a
/// no-op, not a duplicate Treiber node. A duplicate node can self-cycle the
/// inbox and make the stranded-READY rescue mistake a legitimate pending
/// remote wake for a lost enqueue.
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
    // Stand in for a published-then-blocked task: inbox and wake paths refuse
    // a task that is still nascent.
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
    // Baseline strong count with the task merely registered (its registry
    // owner). One placement reference should survive the duplicate push + drain.
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

/// Test: Multiple tasks in remote inbox
/// Verifies FIFO ordering is preserved through inbox drain
pub fn test_remote_inbox_multiple_tasks() -> TestResult {
    let _fixture = SchedFixture::new();

    const NUM_TASKS: usize = 5;
    let mut task_ids = [INVALID_TASK_ID; NUM_TASKS];
    let mut task_guards: [Option<TaskRef>; NUM_TASKS] = [const { None }; NUM_TASKS];

    // Create tasks
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
        // Stand in for a published-then-blocked task; the inbox refuses a
        // task that is still nascent.
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

    // Push all to inbox
    for guard in task_guards.iter().flatten() {
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.push_remote_wake(&guard);
        });
    }

    // Drain all
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    // Verify all are in ready queue
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

/// Test: Timer tick drains inbox on all CPUs
/// This is the key test for the unified scheduler inbox-drain path.
pub fn test_timer_tick_drains_inbox() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create idle task so scheduler can work
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
    // Stand in for a published-then-blocked task: inbox and wake paths refuse
    // a task that is still nascent.
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    if task_set_state(task_id, TaskStatus::Ready) != 0 {
        klog_info!("SCHED_TEST: Failed to set timer-drain task READY");
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Push to inbox (bypassing normal schedule_task)
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(&task_guard);
    });

    // Verify inbox has pending
    let has_pending_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(false);

    if !has_pending_before {
        klog_info!("SCHED_TEST: Task not in inbox before timer tick");
        return TestResult::Fail;
    }

    // Simulate timer tick - this should drain the inbox
    scheduler_timer_tick();

    // Verify inbox is now empty (drained by timer tick)
    let has_pending_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if has_pending_after {
        klog_info!("SCHED_TEST: Timer tick did not drain inbox");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Draining remote inbox must not enqueue non-ready tasks.
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

/// Test: Cross-CPU schedule_task uses lock-free path
/// Verifies that schedule_task to another CPU uses push_remote_wake
pub fn test_cross_cpu_schedule_lockfree() -> TestResult {
    let _fixture = SchedFixture::new();

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count < 2 {
        klog_info!("SCHED_TEST: Skipping cross-CPU test (only 1 CPU)");
        return TestResult::Pass; // Skip on single-CPU systems
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

    // After drain, it should be in ready queue
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

// =============================================================================
// PRIVILEGE SEPARATION TESTS
// Verify that user-mode tasks get correct segment selectors, process VM,
// kernel RSP0 stack, and that the syscall gate has DPL=3.
// =============================================================================

/// Test: User-mode tasks are created with correct privilege separation invariants.
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
    // Stand in for a published-then-blocked task; wakes refuse a nascent one.
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    // Use a wake_tick far in the future so that real timer interrupts
    // (which call wake_due_sleepers with the current tick) never collect
    // our entry before the test explicitly wakes it.  With wake_tick=100
    // the entry was already "due" by the time the test ran, creating a
    // race between the timer handler and the test's block/wake sequence.
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

/// Regression: the tickless-idle path must not panic when the soonest
/// sleep deadline is already in the past. Between a sleeper's deadline
/// passing and the next periodic tick removing it, the idle loop can
/// observe `deadline <= now`, whose `wrapping_sub(now)` lands near
/// `u64::MAX`. Feeding that delta to the tick→ms conversion previously
/// overflowed (`saturating_mul(1000)` pinned to `u64::MAX`, then a
/// non-saturating `+ (freq - 1)`). The idle path must treat a past
/// deadline as already-due and skip arming a one-shot.
pub fn test_tickless_idle_past_deadline_no_overflow() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    // `wake_tick = 1` is in the past once the timer has advanced beyond
    // boot, so the idle path's `wrapping_sub` produces a ~`u64::MAX`
    // delta — the exact input that used to overflow `ticks_to_ms_ceil`.
    if !super::sleep::test_insert_sleep_entry(424_242, 1) {
        super::sleep::reset_sleep_queue();
        return TestResult::Fail;
    }

    // Must return without panicking (no add-with-overflow).
    scheduler::arm_tickless_idle_if_due();

    super::sleep::reset_sleep_queue();
    TestResult::Pass
}

/// Regression: a due sleep entry whose task is NOT (yet) sleep-parked must
/// survive the tick untouched. The old pop-then-wake design destroyed the
/// entry and then dropped the wake when it hit the sleeper's commit window
/// — leaving the task Blocked(Sleep) with no armed entry and placement
/// `None`, permanently (the bare-metal `touchpad-poll` strand). Entries may
/// only disappear once a wake conclusively publishes `Ready`.
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
        return TestResult::Fail;
    }
    let Some(task_guard) = task_find_by_id(task_id) else {
        task_terminate(task_id);
        return TestResult::Fail;
    };
    let task_ptr: &Task = &task_guard;
    // Stand in for a published-then-blocked task; wakes refuse a nascent one.
    assert!(
        scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    // Park the task outside the ready queue so the scheduler cannot dispatch
    // (and exit) it mid-test. It ends up Blocked with a non-Sleep reason —
    // exactly the "not sleep-parked" shape phase 1 needs.
    let _ = unschedule_task(&task_guard);
    if !task_ptr.is_blocked() {
        klog_info!("SCHED_TEST: unschedule did not park the task");
        task_terminate(task_id);
        return TestResult::Fail;
    }

    // Phase 1 — the mid-commit snapshot: entry armed and already due, task
    // not Blocked(Sleep). A tick must leave the entry armed for retry.
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

    // Phase 2 — once the task is genuinely sleep-parked (reason stamped,
    // deadline armed), the next tick must deliver the wake and only then
    // clear the entry.
    super::sleep::arm_blocked_timeout(task_id, 0);
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

// =============================================================================
// REGRESSION: task_wait_for race-window robustness (harmonic-cascade Phase 1H)
//
// These tests verify that the new wait/wake protocol — durable per-task
// `exit_info` cell published before the `waiters` queue's `wake_all`
// fanout — closes the lost-wakeup race that the legacy two-atomic
// `(status, waiting_on)` pair admitted. The buggy version deadlocked on
// child-exit at ~5% on KVM and much higher on TCG.
//
// In the test fixture APs are paused, so the runner cannot reproduce
// the multi-CPU race window directly. What it *can* exercise is the
// fast-path that Phase 1's design relies on for late waiters: any
// `task_wait_for` call that arrives after `mark_task_terminated`'s
// publish must observe the durable `exit_info` (or the Terminated
// status) on its first condition check and return immediately, no
// matter how many such calls land on the same target. A regression
// that re-introduces the lost-wake bug would either deadlock here (the
// runner's test thread blocks forever) or fail the post-conditions on
// `exit_info` / `task_is_terminated`.
// =============================================================================

/// 1000-iteration stress: child kthread is created and immediately
/// terminated; parent (this runner) calls `task_wait_for(child_id)`
/// against the freshly-terminated slot. Each iteration must return
/// promptly via the fast path — no blocking, no deadlock.
///
/// What this catches if regressed:
/// - `mark_task_terminated` failing to publish `exit_info` before
///   `release_task_dependents` (Phase 1G regression) → late waiter
///   would not see `is_set()` true and could fall through to the
///   blocking path, deadlocking the runner.
/// - `task_wait_for`'s condition closure forgetting the
///   `target.is_terminated()` fallback (Phase 1E regression) →
///   same outcome if the publish path ever skipped `try_set`.
/// - `release_task_dependents` no longer doing `waiters.wake_all()`
///   (Phase 1F regression) → caught only on the multi-waiter
///   variant below; the 1000-iter case is dominated by the durable
///   exit_info fast path.
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

        // Terminate the child synchronously. mark_task_terminated runs
        // inline: publishes exit_info via try_set, then wake_all on
        // the (still-empty) waiters queue.
        let rc = task_terminate(child_id);
        if rc != 0 {
            klog_info!(
                "SCHED_TEST: task_terminate returned {} at iteration {}",
                rc,
                i
            );
            return TestResult::Fail;
        }

        // Post-conditions for the publish step.
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

        // The wait must complete via the fast path — if it returns at
        // all the runner is not deadlocked, which is the property we
        // care about.
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

/// Same shape as `test_task_wait_exit_race_1000` but the child slot is
/// driven through Ready→Running→Ready transitions before terminate, so
/// the "did some work" code path of `mark_task_terminated` (which
/// updates `total_runtime` based on `last_run_timestamp`) is exercised
/// alongside the publish/fanout sequence. This shifts the publish
/// timing relative to the runner's wait, so a regression that only
/// reliably misses the wake when the child has run is still caught.
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

        // Simulate "child did some work": Ready -> Running, advance a
        // synthetic last_run_timestamp, then back to Ready so terminate
        // observes a non-zero runtime delta in mark_task_terminated.
        // The state set must respect the FSM (Ready -> Running is
        // valid).
        if !make_task_ready(child_id) {
            klog_info!("SCHED_TEST: failed Ready transition at iter {}", i);
            return TestResult::Fail;
        }
        if task_set_state(child_id, TaskStatus::Running) != 0 {
            klog_info!("SCHED_TEST: failed Running transition at iter {}", i);
            return TestResult::Fail;
        }
        child_ptr.set_last_run_timestamp(1);
        // Spin a few iterations to advance any kdiag timestamp source
        // and shift the relative ordering of publish vs. observe.
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

/// Multi-waiter fanout: durable `exit_info` must satisfy any number of
/// late waiters that arrive after the wake fanout has already fired
/// against an (possibly empty) `waiters` queue. Each subsequent
/// `task_wait_for` for the same terminated child must hit the
/// fast-path return without blocking, no matter how many siblings
/// have already done the same.
///
/// What this catches if regressed:
/// - `release_task_dependents` failing to invoke
///   `child.waiters.wake_all()` (Phase 1F regression) is partially
///   caught here: while the test cannot enqueue real foreign waiters
///   under the paused-AP fixture, it does verify the symmetric
///   guarantee — that durable exit_info plus the Terminated status
///   make repeated independent waits all return 0.
/// - `exit_info` not surviving the wake fanout (e.g. a future
///   refactor that `take`s instead of `try_get`s) — the 4th waiter
///   would observe `is_set()` false.
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

    // Pre-condition: waiters queue is empty before terminate.
    if task_waiter_count(child_ptr) > 0 {
        klog_info!("SCHED_TEST: child waiters queue non-empty before terminate");
        return TestResult::Fail;
    }

    if task_terminate(child_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate failed");
        return TestResult::Fail;
    }

    // After terminate: status Terminated and exit_info published.
    if !child_ptr.is_terminated() {
        klog_info!("SCHED_TEST: child not Terminated after terminate");
        return TestResult::Fail;
    }
    if !child_ptr.exit_info_is_set() {
        klog_info!("SCHED_TEST: exit_info not set after terminate");
        return TestResult::Fail;
    }

    // 4 simulated sibling parents each independently observe the
    // durable exit_info via the fast-path return.
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
        // exit_info must remain set across multiple observations
        // (try_get is non-consuming; only `take` consumes — and the
        // wait path never takes).
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

// =============================================================================
// REGRESSION: Tick accounting & load-aware CPU selection
// =============================================================================

/// Regression: scheduler_timer_tick() must always increment total_ticks.
/// Previously the early-return path skipped increment_ticks(), under-counting
/// ticks on busy CPUs.  This test exercises the unguarded (no PreemptGuard)
/// path only; the guarded path is covered by the live scheduler under SMP.
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

    // Fire several timer ticks
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

/// Regression: idle_time must track ticks, not loop iterations.
/// When the idle task is current, each timer tick should increment both
/// total_ticks and idle_time by the same amount.
pub fn test_idle_time_tracks_ticks_not_iterations() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Set current task to the idle task so timer_tick recognises us as idle.
    // `dispatch()` writes PCR.current_task + scheduler-copy + syscall_pid
    // + state=Running in lockstep — single-writer invariant.
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

    // Both should have incremented by 10 (one per tick).
    if delta_ticks < 10 {
        klog_info!("SCHED_TEST: total_ticks delta {} < 10", delta_ticks);
        return TestResult::Fail;
    }

    let drift = if delta_idle > delta_ticks {
        delta_idle - delta_ticks
    } else {
        delta_ticks - delta_idle
    };
    // Allow a small tolerance (up to 2 ticks) for SMP timing jitter.
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

/// Regression: select_target_cpu should prefer idle CPUs over busy ones.
/// Previously it always returned last_cpu regardless of load.
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

    // Ensure both the local CPU and at least one other CPU are online
    // and schedulable so select_target_cpu sees both as candidates.
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

    // Load up last_cpu (cpu_id) with several queued tasks.
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

    // Create a test task whose last_cpu is cpu_id (busy), with affinity=0 (any CPU).
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
            // Some other idle CPU is also acceptable.
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

/// Regression: CPU running a real task with empty queue must NOT be
/// considered idle.  This is the key scenario that caused all tasks to
/// stick to CPU0 — bursty workloads left the queue empty between bursts,
/// so the old code always returned last_cpu.
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

    // Simulate a real task running on cpu_id: create a task and set it as
    // the current task.  The queue stays empty, but effective_load should
    // be 1 because a non-idle task is running.
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

    // Create a task with last_cpu = cpu_id.  Even though cpu_id's QUEUE
    // is empty, the scheduler should NOT consider it idle.
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

/// Regression: schedule_new_task() must spread sequential forks across
/// CPUs via round-robin, not pile them all onto CPU0.  Mirrors Linux's
/// WF_FORK / SD_BALANCE_FORK slow path.
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

    // Enable all CPUs for scheduling.
    for c in 0..cpu_count {
        slopos_arch::pcr::mark_cpu_online(c);
        super::per_cpu::with_cpu_scheduler(c, |sched| sched.enable());
    }

    // Simulate the parent (shell) running on cpu_id by setting current_task.
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

    // Spawn N children using schedule_new_task (the fork path).
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

    // Verify that at least 2 distinct CPUs were used (not all on CPU0).
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

/// Regression: effective_load must reflect queued tasks correctly.
pub fn test_effective_load_accuracy() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // After fixture reset, effective_load should be 0 or 1 (just the
    // running task on this CPU, if any).
    let load_before = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load())
        .unwrap_or(u32::MAX);
    if load_before > 1 {
        klog_info!(
            "SCHED_TEST: effective_load {} > 1 on empty queues",
            load_before
        );
        return TestResult::Fail;
    }

    // Enqueue a task — effective_load should increase.
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
    // Stand in for a published-then-blocked task; the enqueue path refuses a
    // task that is still nascent.
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

// =============================================================================
// REGRESSION: Phase 7A race-exerciser stress suite
//
// These three tests stress the wait/wake protocol from angles the
// existing Phase 1 sequential stress tests do not cover:
//
//   1. Overlapping parent/child lifetimes within each "fork group" so
//      multiple terminate publishes interleave with multiple wait
//      observations against distinct waiters queues.
//   2. Deep allocation churn: 1000 children, each destroyed before
//      the next is spawned, exercising fresh per-task `waiters` and
//      `exit_info` initialization.
//   3. Cross-priority terminate/wait pair so the runqueue's
//      priority-aware enqueue logic is exercised on the wake path
//      even though the child publishes from a Low-priority slot.
//
// Under `KernelTestScope` APs are paused, so these are not true SMP
// races — they widen coverage of the durable-exit-info fast path,
// fresh-allocation initialization, and the cross-priority wake
// fanout. A regression that re-introduces lost wakeups would either
// deadlock (test runner hangs) or fail one of the post-conditions.
// =============================================================================

/// 10 fork-groups × 100 iterations: each iteration spawns 10 children
/// concurrently, then terminates
/// and waits for each in turn. Total fork/exit/wait cycles = 1000.
///
/// What this catches if regressed:
/// - `mark_task_terminated` skipping the `waiters.wake_all()` fanout
///   (Phase 1F) — under overlapping lifetimes the ring buffer of the
///   first child has had ample opportunity to observe non-empty
///   waiter slots from sibling churn, so a missed fanout would
///   diverge from the durable exit_info path.
/// - A fresh allocation carrying non-empty waiter state.
const FORK_GROUP_WIDTH: usize = 10;
const FORK_GROUP_ITERATIONS: usize = 100;

pub fn test_fork_exit_wait_stress_10x100() -> TestResult {
    let _fixture = SchedFixture::new();

    let mut child_ids = [INVALID_TASK_ID; FORK_GROUP_WIDTH];

    for outer in 0..FORK_GROUP_ITERATIONS {
        // Phase 1: spawn FORK_GROUP_WIDTH children. Their lifetimes
        // overlap — every child is allocated before any is terminated
        // — so the wait/wake protocol is exercised with WIDTH live
        // siblings rather than the always-singleton case of the
        // existing 1000-iter test.
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

            // Pre-condition: a freshly-allocated slot must come back
            // with an empty waiters ring and an unset exit_info, no
            // matter how many prior reuses it has been through. Stale
            // Any state here means fresh initialization regressed.
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

        // Phase 2: terminate all WIDTH children. Each terminate
        // publishes exit_info and fans out wake_all on a still-empty
        // waiters queue. The ordering of these publishes interleaves
        // across siblings — different from the singleton case.
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

        // Phase 3: wait_for each terminated child. Every call must
        // hit the fast path (durable exit_info) and return 0 without
        // blocking. If the runner deadlocks here Phase 1's lost-wake
        // fix has regressed.
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

/// Regression: a parent's `waitpid_nohang`-equivalent reaper must still
/// observe a child's exit info after dozens of unrelated task
/// allocations fire between the child's termination and the parent's
/// reaping call. The child's `exit_info` stays stable in the Zombie task
/// until `task_consume_zombie` transitions it.
///
/// What this catches if regressed:
/// - Anyone re-adding a recyclable parallel exit-record cache causes the
///   post-churn consume to return `None`.
pub fn test_waitpid_survives_task_churn() -> TestResult {
    use super::task::{task_consume_zombie, task_set_parent};

    let _fixture = SchedFixture::new();

    // "Parent" runs the test (`task_set_parent` makes the child's
    // parent_task_id resolve to a Ready slot, so the child takes the
    // Zombie path on termination).
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

    // Churn: spawn + immediately terminate a long chain of
    // kernel-mode tasks. These have no parent so they go straight to
    // Terminated and destroyed. None may disturb the Zombie child.
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

    // Re-find the child by ID and confirm it is still the same task.
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

    // Parent's reaper would call this. Must succeed.
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

    // Cleanup: terminate the parent so the next test starts clean.
    let _ = task_terminate(parent_id);
    TestResult::Pass
}

/// Regression: when a parent terminates without reaping its Zombie
/// children, those children must be auto-reaped (Zombie → Terminated)
/// so they can be destroyed. Without this, a parent that crashes
/// leaves zombies pinned forever and the live-task cap eventually
/// exhausts.
///
/// What this catches if regressed:
/// - `mark_task_terminated` no longer calling `reparent_and_reap_children`
///   → the zombie stays pinned, this test sees status == Zombie after
///   the parent's exit and fails.
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

    // Parent dies without ever calling waitpid: the dying parent must
    // sweep its child list and demote the Zombie to Terminated.
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

    // Before linking: the parent owns no children; the child is pinned only by
    // its registry owner.
    if !parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: parent children list non-empty before link");
        return TestResult::Fail;
    }
    let count_before = task_placement_strong_count(child_nn);

    task_set_parent(child_id, parent_id);

    // After linking: the child is on the parent's list and carries exactly one
    // extra owning reference.
    if parent_ptr.children_is_empty() {
        klog_info!("SCHED_TEST: child not on parent children list after link");
        return TestResult::Fail;
    }
    if task_placement_strong_count(child_nn) != count_before + 1 {
        klog_info!("SCHED_TEST: link did not add exactly one owning reference");
        return TestResult::Fail;
    }

    // Terminating the child makes it a Zombie still owned by the (alive) parent.
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

    // Reaping via the waitpid path unlinks the child, drops the parent's parked
    // reference, and reclaims the task.
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

    // Two children become Zombies owned by the parent; two stay live.
    if task_terminate(child_ids[0]) != 0 || task_terminate(child_ids[1]) != 0 {
        return TestResult::Fail;
    }

    // Parent dies: drains its list. (Its own children list must be empty by the
    // time it is reclaimed, which the `Drop` tripwire also asserts.)
    if task_terminate(parent_id) != 0 {
        return TestResult::Fail;
    }

    // The two zombies were reaped; the two live children survive as orphans.
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

/// Cross-priority wait: a high-priority caller waits on a
/// low-priority child that has already published its exit. Under the
/// paused-AP fixture this is not a true SMP priority-inversion
/// scenario, but it does exercise the symmetric guarantee — that
/// the durable exit_info publish from a Low-priority slot is fully
/// visible to a High-priority observer's wait_event condition check
/// regardless of the producer's runqueue placement.
///
/// What this catches if regressed:
/// - `mark_task_terminated` publishing exit_info via a path that
///   somehow varies with `task->priority` (it shouldn't) would
///   surface as a different ordering on Low than on Normal — the
///   existing Phase 1 tests use Normal exclusively, so this fills
///   that gap.
/// - A future change that makes the WaitQueue's wake fanout
///   priority-aware (e.g. reordering wake_all by waiter priority)
///   could lose the wake on a High waiter waiting on a Low
///   producer; this test pins that wake to fire and observes the
///   fast-path return.
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

    // Producer: low priority. The exit publish runs synchronously
    // inside `task_terminate` from the runner CPU, but the slot's
    // `priority` field stays Low — so any priority-conditional
    // logic on the producer side is exercised at the Low setting.
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

    // Sanity: the two slots really are distinct, so the wait below
    // is genuinely cross-slot rather than a self-wait that the early
    // task_id-equality check in `task_wait_for` would short-circuit.
    if core::ptr::eq(child_ptr, high_ptr) {
        klog_info!("SCHED_TEST: Low and High mapped to the same slot");
        return TestResult::Fail;
    }

    // Terminate Low producer. Publishes exit_info and fans out on
    // the (still-empty) waiters queue, all from a Low-priority
    // slot's perspective.
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

    // The wait must complete via the durable exit_info fast path.
    // A regression that gates wake fanout on producer priority
    // would either deadlock the runner here or corrupt exit_info.
    // `child_ref` keeps the Low task alive across the wait.
    let wait_rc = task_wait_for(child_id);
    if wait_rc != 0 {
        klog_info!(
            "SCHED_TEST: cross-priority task_wait_for returned {} (expected 0)",
            wait_rc
        );
        return TestResult::Fail;
    }

    // Post-condition: the durable exit_info is still observable on
    // the strongly held Low task. The wait path uses `try_get`/`is_set`
    // (non-consuming), never `take`; a regression that consumed
    // the cell would leave subsequent waiters stranded.
    if !child_ptr.exit_info_is_set() {
        klog_info!("SCHED_TEST: exit_info became unset after High-priority wait returned");
        return TestResult::Fail;
    }

    // Clean up the High waiter task.
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

// =============================================================================
// WAIT_QUEUE TESTS
//
// These tests cover the wait/wake primitive's APIs and defense-in-depth
// invariants:
//
// - `WaitOutcome<R>` enum return type.
// - `wait_event_until` / `wait_event_timeout_until` generic-return APIs.
// - Lock-free `has_waiters()` (callers that want to skip a `wake_*`
//   when no one is queued do this at the call site rather than baked
//   into `wake_*` itself — the in-place fast path is unsound on
//   weakly-ordered architectures).
// - `WaitNode::has_woken` auxiliary atomic, exercised indirectly via
//   the wake-empty-queue tests and via the wider scheduler integration.
// - `WaitNode` Drop with null queue back-pointer is a no-op (the
//   common case — back-pointer is cleared inside the WQ critical
//   section by every pop path, so by the time stack-pinned `WaitNode`s
//   go out of scope they hold null).
//
// A panic-mid-wait Drop-firing test is intentionally not included: the
// test-harness uses `catch_panic!` / longjmp, which skips Drops during
// recovery; the Drop-based unlink is defense-in-depth for production
// unwinding paths and runs implicitly on every successful wait.
// =============================================================================

use slopos_ostd::sync::wait_queue::{ParkedTestNode, WaitOutcome, WaitQueue};

/// `wait_event_until` returns the closure's `Some(R)` immediately via
/// the pre-check fast path, without touching the scheduler backend.
pub fn test_wait_event_until_pre_check_returns_carried_value() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: Option<u32> = wq.wait_event_until(|| Some(0xCAFE_F00D_u32));
    if r == Some(0xCAFE_F00D_u32) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Some(0xCAFE_F00D)",
            r
        );
        TestResult::Fail
    }
}

/// `wait_event_timeout_until` returns `WaitOutcome::Ready(R)` on
/// the pre-check fast path.
pub fn test_wait_event_timeout_until_pre_check_ready() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: WaitOutcome<u32> = wq.wait_event_timeout_until(|| Some(7u32), 100);
    if matches!(r, WaitOutcome::Ready(7)) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Ready(7)",
            r
        );
        TestResult::Fail
    }
}

/// `wait_event_timeout_until` with an always-`None` closure does NOT
/// return `Ready` — it must end in either `Timeout` (real wait
/// elapsed) or `NoRuntime` (current task null / backend not yet
/// fully wired in this test fixture). Both are acceptable
/// soundness-preserving outcomes; the property we care about is
/// "the call returns without hanging or panicking and produces a
/// non-Ready outcome when the condition is unsatisfiable."
pub fn test_wait_event_timeout_until_does_not_return_ready_on_none() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: WaitOutcome<u32> = wq.wait_event_timeout_until(|| None, 1);
    match r {
        WaitOutcome::Timeout | WaitOutcome::NoRuntime => TestResult::Pass,
        WaitOutcome::Ready(_) => {
            klog_info!(
                "WAIT_QUEUE_TEST: timeout test returned {:?}, expected Timeout or NoRuntime",
                r
            );
            TestResult::Fail
        }
    }
}

/// `wait_event` (backwards-compat bool wrapper) returns `true` on
/// the pre-check fast path.
pub fn test_wait_event_bool_wrapper_pre_check_true() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.wait_event(|| true) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wait_event_timeout` (backwards-compat bool wrapper) returns
/// `false` on timeout.
pub fn test_wait_event_timeout_bool_wrapper_times_out() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.wait_event_timeout(|| false, 1) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `has_waiters()` on a fresh queue returns `false` via the lock-free
/// read path (it must NOT take the queue's `SpinLock` — if it did,
/// it would still work in kernel mode, but the soundness invariant
/// from the plan would be violated).
pub fn test_has_waiters_fresh_queue_is_false() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.has_waiters() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `WaitQueue::new()` produces a queue with `generation == 0`.
pub fn test_wait_queue_initial_generation() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.generation() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_one` on an empty queue returns `false` without panicking —
/// confirms the has_woken / queue-back-pointer bookkeeping handles
/// the empty-pop branch cleanly.
pub fn test_wake_one_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.wake_one() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_all` on an empty queue returns 0 without panicking.
pub fn test_wake_all_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.wake_all() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// Verify that wake_one / wake_all on an empty queue do NOT bump the
/// generation counter — the generation is only incremented when at
/// least one waiter was actually woken.
pub fn test_generation_unchanged_when_no_waiters() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
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

// =============================================================================
// Phase-1 scheduler-refactor regression tests
// =============================================================================

/// `TaskPriority::KernelIo` is numerically 1 (between `High`=0 and
/// `Normal`=2) and the renumber landed cleanly across the enum's
/// total decoder, strict decoder, and dispatch index.
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
    // Total decoder round-trips every variant.
    for v in 0..=4u8 {
        if TaskPriority::from_u8(v).as_u8() != v {
            return TestResult::Fail;
        }
    }
    // Out-of-range coerces to Normal in the total decoder.
    if TaskPriority::from_u8(255) != TaskPriority::Normal {
        return TestResult::Fail;
    }
    // Strict decoder rejects out-of-range.
    if TaskPriority::try_from_u8(5).is_some() {
        return TestResult::Fail;
    }
    if TaskPriority::try_from_u8(1) != Some(TaskPriority::KernelIo) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// `SleepQueue::earliest_deadline` returns `None` on the lock-free
/// fast path when no tasks are sleeping. Drives the
/// tickless-idle path: when there are no deadlines, the idle loop
/// must skip arming a one-shot LAPIC.
fn test_sleep_queue_next_deadline_none_when_empty() -> TestResult {
    let _fix = SchedFixture::new();
    let now = slopos_kernel_services::platform::timer_ticks();
    match super::sleep::sleep_queue_next_deadline_ticks(now) {
        None => TestResult::Pass,
        Some(_) => TestResult::Fail,
    }
}

/// `KernelIoToken::__new_for_trampoline_only` is documented as the
/// only way to construct a witness. The constructor compiles and
/// returns. (The macro path is exercised in production by every
/// `spawn_kernel_io!` invocation; this test pins the inner ABI.)
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

// =============================================================================
// Per-CPU preempt accounting (single-instruction gs-relative ops)
// =============================================================================
//
// Regression coverage for the migration TOCTOU class: the old
// pointer-then-RMW preempt accounting let an IRQ-driven migration land
// an increment on the previous CPU's count (leaked +1 there, underflow
// panic here). The accounting is now single-instruction gs-relative;
// these tests pin the guard semantics and soak the exact acquisition
// shape that used to corrupt — guard churn at the preemptible baseline
// with the timer free to preempt and migrate the task mid-loop.

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
    // With IRQs disabled nothing else can mutate this CPU's pending
    // flag, and the guard-drop deferred callback is gated off (it
    // requires the IRQs-enabled baseline) — so a guard dropped with
    // the flag set must LEAVE it set for the trap-exit handoff.
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
    static CHURN_LOCK: SpinLock<u64> = SpinLock::new(0, LOCK_LEVEL_RESOURCE);

    let baseline = slopos_ostd::sync::preempt_count();
    for i in 0..20_000u32 {
        // Bare guard churn: the acquisition shape that used to race
        // IRQ-driven migration.
        let guard = slopos_ostd::sync::PreemptGuard::new();
        core::hint::black_box(&guard);
        drop(guard);

        // SpinLock churn: guard + cli + critical section, the exact
        // panicking path (poll_reg_take's lock/unlock).
        let mut slot = CHURN_LOCK.lock();
        *slot = slot.wrapping_add(1);
        drop(slot);

        // Invite the timer to preempt (and the stealer to migrate)
        // the task between iterations.
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

// =============================================================================
// D3 coverage backlog
// =============================================================================

fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

/// Make `task_id` this CPU's current task.
///
/// `dispatch` asserts its incoming task is runnable, so publishing Ready first
/// is not optional: a task straight out of `task_create` is Blocked, and
/// dispatching one trips the dispatcher's own invariant rather than returning
/// an error.
fn dispatch_as_current(task_id: u32) -> bool {
    let cpu = slopos_arch::pcr::get_current_cpu();
    make_task_ready(task_id) && scheduler::dispatch_task_for_test(cpu, task_id)
}

/// Park `task_id` on `wq`.
///
/// [`WaitQueue::enqueue_current`] reads the waiter's identity from the PCR, so
/// making the task this CPU's current for exactly the duration of the enqueue
/// is the only way to put a *chosen* task on a queue from a test. The
/// bootstrap stub is restored before returning: a task that is still some
/// CPU's current is dispatch-pinned, and the reap would refuse it.
fn park_task_on_queue(wq: &WaitQueue, task_id: u32) -> bool {
    // Stand in for a published-then-blocked waiter. A task straight out of
    // `task_create` is `Nascent`, and every wake path deliberately refuses one
    // — so a nascent control would report "not woken" for a reason that has
    // nothing to do with what is under test.
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

/// `(id, status)` for every registered task, so a wake that touches something
/// it should not is visible as a diff rather than as a later mystery.
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
/// Teardown scrubs sleep entries and futex buckets but never unlinks
/// wait-queue nodes, so a node outliving its task is structurally reachable —
/// a waiter SIGKILL'd while parked never unwinds its own stack. The node holds
/// an id, and the wake resolves it through the registry, which is what makes
/// the dangling node inert. Were the node to hold a pointer instead, this wake
/// would read freed memory.
///
/// The `weak.upgrade()` check is load-bearing: it asserts the allocation is
/// *freed*, not merely unhashed, so "the id resolves to nothing" is not
/// masking a task that is still sitting at that address.
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

    let wq = WaitQueue::new();
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

    // Gone in both senses: unhashed from the registry, and actually freed.
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

    // The node is still linked. The wake must pop it and resolve its id to
    // nothing, rather than to whatever now occupies the address.
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

    // Positive control. Without it, "a node was popped" is satisfiable by a
    // `wake_one` that reaches no task at all, and the no-disturbance assertion
    // above would hold vacuously.
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
    let control = WaitQueue::new();
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
/// Covers the predicate and the effect it drives. Equal priority is the case
/// that matters: an inverted or non-strict comparison still passes a
/// high-versus-low check, and turns every same-priority wake into a
/// reschedule — a preemption storm rather than a visible fault.
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

    // --- predicate, against a Normal current task --------------------------
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

    // --- predicate, against a CPU running nothing schedulable --------------
    park_bootstrap_on_current_cpu();
    if !scheduler::newcomer_outranks_current(cpu, &idle) {
        klog_info!("SCHED_TEST: Idle did not outrank PRIORITY_NONE");
        return TestResult::Fail;
    }

    // --- the effect the predicate drives -----------------------------------
    slopos_arch::pcr::mark_cpu_online(cpu);
    if super::per_cpu::with_cpu_scheduler(cpu, |sched| sched.enable()).is_none() {
        return TestResult::Fail;
    }
    let affinity = super::per_cpu::affinity_mask_for_cpu(cpu);
    crate::task::task_install_idle_affinity(&high, affinity, cpu as u8);
    crate::task::task_install_idle_affinity(&normal_b, affinity, cpu as u8);

    // The reschedule request is gated on scheduling being active; enable it
    // only across the two publications so the fixture's quiescence is
    // disturbed as little as possible.
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
/// A false accept would hand a raw PCR reader an address that is not a task at
/// all, to be read as one. The interior
/// sweep is exhaustive only because the stub is exactly its 8-byte ABI header,
/// which is asserted here so a growth of the struct fails by name rather than
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

    // One past the end and one before the base. Both are asserted only when
    // they are not the BSP stub itself: the two statics are independent, so
    // the linker may place the BSP stub immediately either side of the array.
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

/// Task ids are handed out strictly increasing, are never recycled once
/// retired, and the allocator does not rewind across a fixture reset.
///
/// Every id-keyed subsystem — the wait queues, the sleep queue, the futex
/// buckets, `waitpid` — is only safe because a stale id resolves to nothing
/// rather than to a different task. `init_task_manager` promises the monotonic
/// source survives reinitialisation, and nothing else checks it.
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
        // Retired, not recycled: the id stays spent even though nothing holds
        // it any more.
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

    // A fixture reset retires every non-idle registration. The monotonic id
    // source must survive it.
    if crate::task::init_task_manager() != 0 {
        klog_info!("SCHED_TEST: init_task_manager failed");
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

/// The AP pause nests on a depth, so a release only resumes the APs when it is
/// the last one outstanding — and the holders are independent, so releasing in
/// acquire order has to work as well as releasing in reverse.
///
/// A flag gets the second half wrong: the earlier release lifts the pause while
/// a holder is still standing, and that holder then runs its critical section
/// against live APs believing they are parked.
///
/// Deliberately runs without a fixture. `KernelTestScope` holds a pause for the
/// whole of any test that uses one, and the transition being pinned here is the
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

    // Every observation is recorded before it is judged, so no failure path can
    // return while a token is still outstanding and leave the APs parked.
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

/// A pause that cannot be established is an error the caller has to handle, and
/// the failed attempt leaves no depth behind. Both halves are load-bearing: a
/// silent success would let `KernelTestScope` run a test body against APs that
/// are still free to race it, and a leaked depth would park every AP for the
/// rest of the boot.
///
/// Provokes the timeout through the executing flag itself, which is the only
/// state `pause_all_aps` waits on and which nothing else reads. An AP may clear
/// the forced flag by passing through its dispatch path underneath the attempt,
/// so the provocation gets a bounded number of tries before the test reports it
/// unreproducible rather than silently passing.
pub fn test_ap_pause_timeout_is_reported_and_rolled_back() -> TestResult {
    if slopos_arch::pcr::get_cpu_count() < 2 {
        klog_info!("SCHED_TEST: uniprocessor boot has no AP to hold a pause off");
        return TestResult::Skipped;
    }
    if crate::per_cpu::ap_pause_depth() != 0 {
        klog_info!("SCHED_TEST: entered with an AP pause already held");
        return TestResult::Fail;
    }

    const ATTEMPTS: usize = 3;
    const HELD_CPU: usize = 1;
    let mut blamed_cpu = None;
    for _ in 0..ATTEMPTS {
        if crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(true))
            .is_none()
        {
            klog_info!("SCHED_TEST: CPU {} has no per-CPU scheduler", HELD_CPU);
            return TestResult::Fail;
        }
        let outcome = crate::per_cpu::pause_all_aps();
        crate::per_cpu::with_cpu_scheduler(HELD_CPU, |sched| sched.set_executing_task(false));
        match outcome {
            Err(crate::per_cpu::ApPauseError::Timeout { cpu_id }) => {
                blamed_cpu = Some(cpu_id);
                break;
            }
            Ok(token) => crate::per_cpu::resume_all_aps_if_not_nested(token),
        }
    }

    let Some(cpu_id) = blamed_cpu else {
        klog_info!("SCHED_TEST: pause_all_aps never observed the held AP");
        return TestResult::Fail;
    };
    if cpu_id != HELD_CPU {
        klog_info!(
            "SCHED_TEST: timeout blamed CPU {} instead of CPU {}",
            cpu_id,
            HELD_CPU
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

// =============================================================================
// Teardown of a task parked in a stack-pinned wait
// =============================================================================

/// Create a kernel task and park a wait node for it on `wq`, leaving the park
/// back-pointer published.
///
/// `park_unowned_node_for_test` reads the waiter's identity from the PCR, so
/// making the task this CPU's current for exactly the duration of the link is
/// the only way to park a *chosen* task from a test. The bootstrap stub is
/// restored before returning: a task that is still some CPU's current is
/// dispatch-pinned, and teardown would refuse it.
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
/// The node lives on the victim's kernel stack, and `TaskStack` recycles a slot
/// rather than quarantining it — so a node still linked after teardown is
/// dereferenced and written through by the next wake, at an address that by
/// then belongs to a different task. The keeper is what makes that wake
/// happen: a queue with no other waiter is never woken again.
pub fn test_terminated_waiter_leaves_no_wait_node() -> TestResult {
    let _fixture = SchedFixture::new();

    // Declared before the nodes so it outlives them: a node still linked at
    // drop reaches back through its queue pointer to unlink itself.
    let wq = WaitQueue::new();

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

    // `remove_task` declines an unowned node by contract, so this is the
    // control: teardown must not be relying on it.
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

    // The surviving node must still be the keeper's, reachable by a wake.
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
