//! Hermetic kernel-test scope guard.
//!
//! `KernelTestScope` sets up a clean scheduler state for a kernel test and
//! restores the kernel-wide singleton state on Drop; every kernel-test fixture
//! in the workspace delegates to it. The singletons come from the
//! `slopos_hermetic` linker-section registry, so a subsystem declares one
//! `hermetic_state! { ... }` block (see `crate::test_hermetic`) rather than
//! editing this scope.
//!
//! The AP pause, the inbox drain and the RCU barrier are owned here instead:
//! they serialise the capture window rather than being snapshotable state.

/// No-op placeholder for tests that only care about the task struct, not the
/// body. `extern "C"` to match the `TaskEntry` alias the scheduler exports.
pub extern "C" fn dummy_task_entry(_arg: *mut core::ffi::c_void) {}

use core::marker::PhantomData;

use slopos_hermetic::{BootCtx, HermeticVTable, TestInit, topo_order};
use slopos_ostd::KVec;
use slopos_ostd::klog_info;
use slopos_ostd::sync::StateFlag;
use slopos_ostd::test_support::hermetic::{
    SnapshotError, run_restore_phase_drain, run_snapshot_phase,
};

/// Guards registration of the panic-cleanup that clears `TEST_SCOPE_ACTIVE`:
/// without it a panicking test leaves the flag set and every later `enter()`
/// panics.
static PANIC_CLEANUP_REGISTERED: StateFlag = StateFlag::new();

fn ensure_panic_cleanup_registered() {
    if PANIC_CLEANUP_REGISTERED.is_active() {
        return;
    }
    PANIC_CLEANUP_REGISTERED.set_active();
    slopos_ostd::panic_recovery::register_panic_cleanup(panic_clear_test_scope);
}

fn panic_clear_test_scope() {
    slopos_hermetic::clear_test_scope_after_panic();
    // Before the thaw, so a held thread is queued again by the time the freeze releases it.
    if let Some(held) = slopos_ostd::sync::kernel_io_task::clear_kernel_io_hold_after_panic() {
        crate::task::republish_held_kernel_io(&held);
    }
    // Or a panic mid-freeze parks every kernel-I/O thread for the rest of boot.
    slopos_ostd::sync::kernel_io_task::clear_kernel_io_freeze_after_panic();
}

use super::per_cpu::{
    ApPauseToken, clear_all_cpu_queues, pause_all_aps, resume_all_aps_if_not_nested,
    with_cpu_scheduler,
};
use super::scheduler::{init_scheduler, scheduler_shutdown};
use super::task::{
    FreezeOutcome, KernelIoFreeze, KernelIoHold, freeze_kernel_io_all, hold_kernel_io_all,
    kernel_io_dispatchable_count, task_registry_reset, task_shutdown_population,
};

/// RAII scope guard for kernel tests that mutate scheduler / task state. Embed
/// it as a fixture field; do not implement Drop on the wrapper — the scope's
/// Drop handles teardown.
pub struct KernelTestScope {
    kernel_io_freeze: Option<KernelIoFreeze>,
    kernel_io_hold: Option<KernelIoHold>,
    aps_paused: Option<ApPauseToken>,
    captured: KVec<(&'static HermeticVTable, core::ptr::NonNull<()>)>,
    boot_ctx: Option<BootCtx<'static, TestInit>>,
    /// !Send !Sync: scope is pinned to the constructing CPU (BSP).
    _not_send: PhantomData<*mut ()>,
}

impl KernelTestScope {
    /// Whether no registered kernel-I/O thread is owned by any scheduler container.
    pub fn kernel_io_is_quiesced(&self) -> bool {
        kernel_io_dispatchable_count() == 0
    }

    /// What the cooperative freeze itself managed; quiescence does not depend on it.
    pub fn kernel_io_freeze_outcome(&self) -> Option<FreezeOutcome> {
        self.kernel_io_freeze.as_ref().map(KernelIoFreeze::outcome)
    }

    /// Threads a container still owned when the hold's settle loop gave up.
    pub fn kernel_io_unsettled(&self) -> usize {
        self.kernel_io_hold
            .as_ref()
            .map_or(0, super::task::KernelIoHold::unsettled)
    }

    /// Enter the scope: freeze the kernel-I/O threads, snapshot kernel-wide
    /// state via the hermetic registry, pause APs, and reset the task
    /// population + scheduler so the test starts from a clean slate.
    ///
    /// Panics if:
    /// - a previous scope is still alive (BootCtx slot empty),
    /// - the APs will not park, since the scope's whole contract is that
    ///   they cannot race the test body,
    /// - `task_registry_reset` or `init_scheduler` returns non-zero,
    /// - the registry has a dependency cycle,
    /// - snapshot allocation fails.
    pub fn enter() -> Self {
        ensure_panic_cleanup_registered();

        // First, so a concurrent scope is rejected before any state is mutated.
        let boot_ctx = slopos_hermetic::take_for_test();

        // Before the AP pause: a thread parks on its gate under its own power.
        let kernel_io_freeze = freeze_kernel_io_all();

        // Every snapshot below reads kernel-wide state an AP is free to mutate,
        // so a scope entered over running APs would report results from a run
        // it did not control.
        // Retried rather than fatal on the first failure: the depth is rolled
        // back on failure, so each attempt starts clean, and an AP the host has
        // simply not scheduled yet may park on the next one. A final failure
        // still panics — the scope's whole contract is that APs cannot race the
        // body, and running it anyway would report a result from a run it did
        // not control. Panicking one test is correct; the defect this plan
        // fixed was one dead CPU panicking thirty-two.
        const PAUSE_ATTEMPTS: usize = 3;
        let mut last_err = None;
        let mut paused = None;
        for attempt in 0..PAUSE_ATTEMPTS {
            if attempt != 0 {
                super::per_cpu::note_ap_pause_retry();
            }
            match pause_all_aps() {
                Ok(token) => {
                    paused = Some(token);
                    break;
                }
                Err(err) => last_err = Some(err),
            }
        }
        let aps_paused = match paused {
            Some(token) => token,
            None => {
                drop(kernel_io_freeze);
                slopos_hermetic::return_after_test(boot_ctx);
                panic!(
                    "KernelTestScope: AP pause failed: {:?} ({} attempts)",
                    last_err, PAUSE_ATTEMPTS
                );
            }
        };

        // After the pause and before the inbox clear: a covered task in an inbox must reach the hold.
        let kernel_io_hold = hold_kernel_io_all(&kernel_io_freeze, &aps_paused);

        // Drop wake-IPIs issued before the pause flag became visible to APs.
        let cpu_count = slopos_arch::pcr::get_cpu_count();
        for cpu in 0..cpu_count {
            let _ = with_cpu_scheduler(cpu, |s| s.force_clear_inbox_count());
        }
        slopos_ostd::sync::synchronize_rcu();

        let order = match topo_order() {
            Ok(o) => o,
            Err(e) => {
                klog_info!("KernelTestScope: registry topo_order failed: {:?}", e);
                drop(kernel_io_hold);
                resume_all_aps_if_not_nested(aps_paused);
                drop(kernel_io_freeze);
                slopos_hermetic::return_after_test(boot_ctx);
                panic!("KernelTestScope: registry topo_order failed");
            }
        };

        let captured = match run_snapshot_phase(order.as_slice()) {
            Ok(captured) => captured,
            Err((mut partial, err)) => {
                let label = match err {
                    SnapshotError::Oom => "KVec push OOM",
                    SnapshotError::StateAllocFailed(name) => name,
                };
                klog_info!("KernelTestScope: snapshot OOM for state {}", label);
                run_restore_phase_drain(&mut partial);
                drop(kernel_io_hold);
                resume_all_aps_if_not_nested(aps_paused);
                drop(kernel_io_freeze);
                slopos_hermetic::return_after_test(boot_ctx);
                panic!("KernelTestScope: snapshot OOM");
            }
        };
        let mut captured = captured;

        // The reset runs after the snapshot, so the captured values are the
        // pre-reset ones and Drop's restore walk puts those back.
        slopos_arch::pcr::park_bootstrap_task(
            slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut (),
        );

        task_shutdown_population();
        scheduler_shutdown();

        let mut reset_failure: Option<&'static str> = None;
        if task_registry_reset(&kernel_io_freeze) != 0 {
            reset_failure = Some("task_registry_reset");
        } else if init_scheduler() != 0 {
            reset_failure = Some("init_scheduler");
        }

        if let Some(stage) = reset_failure {
            klog_info!("KernelTestScope: {} failed", stage);
            run_restore_phase_drain(&mut captured);
            drop(kernel_io_hold);
            resume_all_aps_if_not_nested(aps_paused);
            drop(kernel_io_freeze);
            slopos_hermetic::return_after_test(boot_ctx);
            panic!("KernelTestScope: {} failed", stage);
        }

        // Inbox counts can reappear between the initial drain and the re-init.
        let mut missing_cpu: Option<usize> = None;
        for cpu in 0..cpu_count {
            if with_cpu_scheduler(cpu, |sched| sched.force_clear_inbox_count()).is_none() {
                missing_cpu = Some(cpu);
                break;
            }
        }
        if let Some(cpu) = missing_cpu {
            run_restore_phase_drain(&mut captured);
            drop(kernel_io_hold);
            resume_all_aps_if_not_nested(aps_paused);
            drop(kernel_io_freeze);
            slopos_hermetic::return_after_test(boot_ctx);
            panic!(
                "KernelTestScope: per-CPU scheduler {} missing after init",
                cpu
            );
        }

        Self {
            kernel_io_freeze: Some(kernel_io_freeze),
            kernel_io_hold: Some(kernel_io_hold),
            aps_paused: Some(aps_paused),
            captured,
            boot_ctx: Some(boot_ctx),
            _not_send: PhantomData,
        }
    }

    /// Alias for `enter()`, so `MyFixture::new()` keeps working where
    /// `MyFixture` is a type alias for `KernelTestScope`.
    pub fn new() -> Self {
        Self::enter()
    }

    /// Borrow the `BootCtx` so a test can call boot-only mutators
    /// (`gdt_set_ist`, `init_scheduler`, ...).
    pub fn with_boot<R>(&mut self, f: impl FnOnce(&mut BootCtx<'static, TestInit>) -> R) -> R {
        let ctx = self
            .boot_ctx
            .as_mut()
            .expect("KernelTestScope: BootCtx already consumed");
        f(ctx)
    }
}

impl Drop for KernelTestScope {
    fn drop(&mut self) {
        task_shutdown_population();
        scheduler_shutdown();
        clear_all_cpu_queues();

        run_restore_phase_drain(&mut self.captured);

        if let Some(ctx) = self.boot_ctx.take() {
            slopos_hermetic::return_after_test(ctx);
        }

        // After the restore, so the republish sees the per-CPU enabled bits; before the resume,
        // so the pause release's own IPI sweep kicks the target AP.
        drop(self.kernel_io_hold.take());

        if let Some(token) = self.aps_paused.take() {
            resume_all_aps_if_not_nested(token);
        }

        drop(self.kernel_io_freeze.take());
    }
}
