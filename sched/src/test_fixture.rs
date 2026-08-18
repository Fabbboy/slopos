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
}

use super::per_cpu::{
    ApPauseToken, clear_all_cpu_queues, pause_all_aps, resume_all_aps_if_not_nested,
    with_cpu_scheduler,
};
use super::scheduler::{init_scheduler, scheduler_shutdown};
use super::task::{init_task_manager, task_shutdown_all};

/// RAII scope guard for kernel tests that mutate scheduler / task state. Embed
/// it as a fixture field; do not implement Drop on the wrapper — the scope's
/// Drop handles teardown.
pub struct KernelTestScope {
    aps_paused: Option<ApPauseToken>,
    captured: KVec<(&'static HermeticVTable, core::ptr::NonNull<()>)>,
    boot_ctx: Option<BootCtx<'static, TestInit>>,
    /// !Send !Sync: scope is pinned to the constructing CPU (BSP).
    _not_send: PhantomData<*mut ()>,
}

impl KernelTestScope {
    /// Enter the scope: snapshot kernel-wide state via the hermetic
    /// registry, pause APs, and reinitialise the task manager +
    /// scheduler so the test starts from a clean slate.
    ///
    /// Panics if:
    /// - a previous scope is still alive (BootCtx slot empty),
    /// - the APs will not park, since the scope's whole contract is that
    ///   they cannot race the test body,
    /// - `init_task_manager` or `init_scheduler` returns non-zero,
    /// - the registry has a dependency cycle,
    /// - snapshot allocation fails.
    pub fn enter() -> Self {
        ensure_panic_cleanup_registered();

        // First, so a concurrent scope is rejected before any state is mutated.
        let boot_ctx = slopos_hermetic::take_for_test();

        // Every snapshot below reads kernel-wide state an AP is free to mutate,
        // so a scope entered over running APs would report results from a run
        // it did not control.
        let aps_paused = match pause_all_aps() {
            Ok(token) => token,
            Err(err) => {
                slopos_hermetic::return_after_test(boot_ctx);
                panic!("KernelTestScope: AP pause failed: {:?}", err);
            }
        };

        // Drop wake-IPIs issued before the pause flag became visible to APs.
        let cpu_count = slopos_arch::pcr::get_cpu_count();
        for cpu in 0..cpu_count {
            #[cfg(feature = "test-hooks")]
            let _ = with_cpu_scheduler(cpu, |s| s.force_clear_inbox_count());
            #[cfg(not(feature = "test-hooks"))]
            let _ = cpu;
        }
        slopos_ostd::sync::synchronize_rcu();

        let order = match topo_order() {
            Ok(o) => o,
            Err(e) => {
                klog_info!("KernelTestScope: registry topo_order failed: {:?}", e);
                resume_all_aps_if_not_nested(aps_paused);
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
                resume_all_aps_if_not_nested(aps_paused);
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

        task_shutdown_all();
        scheduler_shutdown();

        let mut reset_failure: Option<&'static str> = None;
        if init_task_manager() != 0 {
            reset_failure = Some("init_task_manager");
        } else if init_scheduler() != 0 {
            reset_failure = Some("init_scheduler");
        }

        if let Some(stage) = reset_failure {
            klog_info!("KernelTestScope: {} failed", stage);
            run_restore_phase_drain(&mut captured);
            resume_all_aps_if_not_nested(aps_paused);
            slopos_hermetic::return_after_test(boot_ctx);
            panic!("KernelTestScope: {} failed", stage);
        }

        // Inbox counts can reappear between the initial drain and the re-init.
        #[cfg(feature = "test-hooks")]
        {
            let mut missing_cpu: Option<usize> = None;
            for cpu in 0..cpu_count {
                if with_cpu_scheduler(cpu, |sched| sched.force_clear_inbox_count()).is_none() {
                    missing_cpu = Some(cpu);
                    break;
                }
            }
            if let Some(cpu) = missing_cpu {
                run_restore_phase_drain(&mut captured);
                resume_all_aps_if_not_nested(aps_paused);
                slopos_hermetic::return_after_test(boot_ctx);
                panic!(
                    "KernelTestScope: per-CPU scheduler {} missing after init",
                    cpu
                );
            }
        }

        Self {
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
        task_shutdown_all();
        scheduler_shutdown();
        clear_all_cpu_queues();

        run_restore_phase_drain(&mut self.captured);

        if let Some(ctx) = self.boot_ctx.take() {
            slopos_hermetic::return_after_test(ctx);
        }

        if let Some(token) = self.aps_paused.take() {
            resume_all_aps_if_not_nested(token);
        }
    }
}
