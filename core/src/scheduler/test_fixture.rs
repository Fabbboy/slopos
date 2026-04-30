//! Hermetic kernel-test scope guard.
//!
//! `KernelTestScope` is the single source of truth for "set up a clean
//! scheduler state for a kernel test, then restore the kernel-wide
//! singleton state on Drop." Every kernel-test fixture in the workspace
//! delegates to this scope so the snapshot/restore logic lives in one
//! place — adding a new fixture means embedding a `KernelTestScope`
//! field, not re-implementing the snapshot.
//!
//! ## Why this exists
//!
//! Multiple kernel-test files declare their own RAII fixtures
//! (`SchedFixture` in `sched_tests.rs`, `ContextFixture` in
//! `context_tests.rs`, `ShutdownFixture` in `shutdown_tests.rs`).
//! Their `new()`s all call `init_scheduler()`, which on its first
//! invocation runs `init_all_percpu_schedulers()` and resets every
//! per-CPU scheduler — including `enabled = false` for APs that boot
//! had previously set to `true`. Their `Drop`s only call
//! `scheduler_shutdown()` (a global flag flip) — they do **not**
//! restore the per-CPU `enabled` bits, the `cpu_online` bits,
//! `PCR.current_task[BSP]`, or `PCR.idle_task[BSP]`.
//!
//! The cumulative effect: by the time the kernel-test phase finishes,
//! APs are stuck `enabled = false`, BSP's `PCR.current_task` still
//! points at the bootstrap stub, and queues hold stale
//! `reset_in_place`-d task pointers. The boot continues into the
//! services phase, which spawns `init`. `find_idlest_cpu` sees no
//! schedulable CPU (all `enabled = false`), falls back to local enqueue
//! on BSP, and `init` waits for BSP's scheduler-loop to start. Even
//! once it does, the userland tests it spawns also target BSP, and the
//! whole userland phase serializes into a thread that may also trip
//! over orphaned BSP `idle/0` tasks left in the pool from a test that
//! called `create_idle_task()`.
//!
//! `KernelTestScope::enter()` snapshots every kernel-wide singleton the
//! wrapped test can mutate, then runs the standard `pause_all_aps` →
//! `task_shutdown_all` → `scheduler_shutdown` → `init_task_manager` →
//! `init_scheduler` → `force_clear_inbox_count` setup. `Drop` runs
//! the inverse: `task_shutdown_all` → `scheduler_shutdown` →
//! `clear_all_cpu_queues` → restore PCR / per-CPU state from snapshot
//! → `resume_all_aps`. APs only un-pause once the world is fully
//! restored, so they never observe a half-restored state.
//!
//! This guard is the **only** scheduler-test setup primitive in the
//! workspace. If a future test needs to mutate state this guard
//! doesn't snapshot, the right move is to extend the snapshot — not
//! to add a parallel fixture or a post-hoc cleanup.

use slopos_utils::klog_info;

use super::per_cpu::{
    clear_all_cpu_queues, pause_all_aps, resume_all_aps_if_not_nested, with_cpu_scheduler,
};
use super::scheduler::{init_scheduler, scheduler_shutdown};
use super::task::{init_task_manager, task_shutdown_all};

/// Maximum CPU index covered by the snapshot bitmaps. Bumping this past
/// 32 requires switching `cpu_online_pre` / `cpu_enabled_pre` from `u32`
/// to a wider bitmap (e.g. `[u32; MAX_CPUS / 32]`). Tests today run with
/// at most QEMU_SMP=4 CPUs so 32 is comfortable headroom.
const SCOPE_BITMAP_MAX_CPUS: usize = 32;

/// RAII scope guard for kernel tests that mutate scheduler / task
/// state. Embed this as a field in a fixture; do not implement Drop on
/// the wrapper — the scope's Drop handles teardown.
pub struct KernelTestScope {
    aps_paused: bool,
    /// Bit `n` set ⇔ CPU `n` was `is_cpu_online == true` before
    /// `enter()` took its snapshot.
    cpu_online_pre: u32,
    /// Bit `n` set ⇔ per-CPU scheduler `n` was `is_enabled == true`
    /// before `enter()` took its snapshot.
    cpu_enabled_pre: u32,
    /// `PCR.current_task[BSP]` pointer captured before `enter()` parked
    /// it on the bootstrap stub.
    bsp_current_task_pre: *mut (),
    /// `PCR.idle_task[BSP]` pointer captured before `enter()`. Tests
    /// that call `create_idle_task()` overwrite this; restoring the
    /// prior pointer (often null on the first scope) lets
    /// `init_task_manager` reset the test-installed idle Task on the
    /// next sweep — `is_idle_task` returns false for a slot no PCR
    /// references, so it falls through to `reset_in_place`. No
    /// orphaned `idle/0` accumulates in the pool.
    bsp_idle_task_pre: *mut (),
}

impl KernelTestScope {
    /// Enter the scope: snapshot kernel-wide state, pause APs, and
    /// reinitialise the task manager + scheduler so the test starts
    /// from a clean slate.
    ///
    /// Panics if `init_task_manager` or `init_scheduler` fails. Those
    /// failures imply unrecoverable kernel-state corruption and aborting
    /// the test run is the only safe response.
    pub fn enter() -> Self {
        let aps_paused = pause_all_aps();

        let mut cpu_online_pre = 0u32;
        let mut cpu_enabled_pre = 0u32;
        let cpu_count = slopos_arch::pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
        for cpu_id in 0..cpu_count {
            if slopos_arch::pcr::is_cpu_online(cpu_id) {
                cpu_online_pre |= 1u32 << cpu_id;
            }
            let enabled = with_cpu_scheduler(cpu_id, |s| s.is_enabled()).unwrap_or(false);
            if enabled {
                cpu_enabled_pre |= 1u32 << cpu_id;
            }
        }
        let bsp_current_task_pre = slopos_arch::pcr::get_current_task_for(0);
        let bsp_idle_task_pre = slopos_arch::pcr::get_idle_task(0);

        // Park PCR.current_task on the BSP SafeStack bootstrap stub
        // BEFORE init_task_manager resets pool tasks in place. Any
        // prior dispatch may have left PCR.current_task pointing at a
        // pool-backed Task that `init_task_manager` is about to
        // `reset_in_place` — reading through it after that zeroes
        // `unsafe_stack_sp` and crashes the next instrumented prologue.
        // The bootstrap stub is not in the pool (whitelisted by
        // `task_pointer_is_valid`) and retains a primed
        // `unsafe_stack_sp` for the lifetime of the kernel image.
        slopos_arch::pcr::set_current_task(super::safestack_rt::BSP_BOOTSTRAP_TASK.get() as *mut ());

        task_shutdown_all();
        scheduler_shutdown();

        if init_task_manager() != 0 {
            klog_info!("KernelTestScope: init_task_manager failed");
            resume_all_aps_if_not_nested(aps_paused);
            panic!("KernelTestScope: init_task_manager failed");
        }
        if init_scheduler() != 0 {
            klog_info!("KernelTestScope: init_scheduler failed");
            resume_all_aps_if_not_nested(aps_paused);
            panic!("KernelTestScope: init_scheduler failed");
        }

        // Force-clear any stale inbox counts that accumulated between
        // the previous scope's Drop and this init (e.g. from AP timer
        // ticks that fired before pause took effect).
        for cpu in 0..slopos_arch::pcr::get_cpu_count() {
            if with_cpu_scheduler(cpu, |sched| sched.force_clear_inbox_count()).is_none() {
                resume_all_aps_if_not_nested(aps_paused);
                panic!("KernelTestScope: per-CPU scheduler missing after init");
            }
        }

        Self {
            aps_paused,
            cpu_online_pre,
            cpu_enabled_pre,
            bsp_current_task_pre,
            bsp_idle_task_pre,
        }
    }
}

impl Drop for KernelTestScope {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();

        // Drain every per-CPU ready queue and remote inbox so any
        // task pointers left behind by the test are released here.
        clear_all_cpu_queues();

        // Restore per-CPU `cpu_online` and `enabled` bitmaps to the
        // pre-scope snapshot. Without this, tests that called
        // `mark_cpu_online(N)` or `with_cpu_scheduler(N, |s| s.enable())`
        // — and the `init_scheduler` call inside `enter()` itself, which
        // resets every per-CPU scheduler's `enabled` to false on its
        // FIRST invocation — leave those bits at their post-init values
        // forever, breaking subsequent boot flow / future test setups.
        let cpu_count = slopos_arch::pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
        for cpu_id in 0..cpu_count {
            let want_online = (self.cpu_online_pre & (1u32 << cpu_id)) != 0;
            if want_online {
                slopos_arch::pcr::mark_cpu_online(cpu_id);
            } else {
                slopos_arch::pcr::mark_cpu_offline(cpu_id);
            }
            let want_enabled = (self.cpu_enabled_pre & (1u32 << cpu_id)) != 0;
            with_cpu_scheduler(cpu_id, |s| {
                if want_enabled {
                    s.enable();
                } else {
                    s.disable();
                }
            });
        }

        // Restore PCR pointers BEFORE resuming APs so a freshly
        // un-paused AP never observes a transient half-restored PCR.
        slopos_arch::pcr::set_idle_task(0, self.bsp_idle_task_pre);
        slopos_arch::pcr::set_current_task(self.bsp_current_task_pre);

        resume_all_aps_if_not_nested(self.aps_paused);
    }
}
