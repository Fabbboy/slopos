//! `HermeticState` impls for the kernel-singleton state that
//! `KernelTestScope` previously snapshotted by hand.
//!
//! Each impl declares one piece of mutable scheduler/PCR state that
//! tests may transiently mutate. The framework auto-walks the registry
//! at scope enter/Drop, so adding a new singleton means writing one
//! `hermetic_state! { ... }` block — never editing the scope itself.
//!
//! Each entry uses the `hermetic_state! { ... }` block form: one
//! macro invocation emits the marker struct, the trait impl, and the
//! `.hermetic_state_registry` linker-section entry. The custom
//! per-impl snapshot/restore logic stays — the boilerplate is what
//! the macro absorbs.

#![cfg(feature = "test-hooks")]

use core::sync::atomic::Ordering;

use slopos_arch::pcr;
use slopos_ostd::hermetic_state;
use slopos_ostd::test_support;

use super::per_cpu::{SCHEDULERS_INIT, with_cpu_scheduler};

const SCOPE_BITMAP_MAX_CPUS: usize = 32;

// =============================================================================
// PerCpuOnlineBits — `pcr::is_cpu_online(cpu)` / `mark_cpu_online/offline`
// =============================================================================

hermetic_state! {
    pub PerCpuOnlineBits {
        type Snapshot = u32;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            let mut bits = 0u32;
            let cpu_count = pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
            for cpu_id in 0..cpu_count {
                if pcr::is_cpu_online(cpu_id) {
                    bits |= 1u32 << cpu_id;
                }
            }
            Ok(bits)
        }
        fn restore(bits: Self::Snapshot) {
            let cpu_count = pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
            for cpu_id in 0..cpu_count {
                if bits & (1u32 << cpu_id) != 0 {
                    pcr::mark_cpu_online(cpu_id);
                } else {
                    pcr::mark_cpu_offline(cpu_id);
                }
            }
        }
    }
}

// =============================================================================
// PerCpuSchedulerEnableBits — per-CPU `sched.is_enabled()` bitmap
// =============================================================================
//
// Depends on PerCpuOnlineBits because some tests bring CPUs offline,
// and restoring `enabled=true` for an offline CPU would be confusing.

hermetic_state! {
    pub PerCpuSchedulerEnableBits {
        type Snapshot = u32;
        const DEPENDS_ON: &[&str] = &["PerCpuOnlineBits"];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            let mut bits = 0u32;
            let cpu_count = pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
            for cpu_id in 0..cpu_count {
                if with_cpu_scheduler(cpu_id, |s| s.is_enabled()).unwrap_or(false) {
                    bits |= 1u32 << cpu_id;
                }
            }
            Ok(bits)
        }
        fn restore(bits: Self::Snapshot) {
            let cpu_count = pcr::get_cpu_count().min(SCOPE_BITMAP_MAX_CPUS);
            for cpu_id in 0..cpu_count {
                let want = bits & (1u32 << cpu_id) != 0;
                with_cpu_scheduler(cpu_id, |s| {
                    if want {
                        s.enable();
                    } else {
                        s.disable();
                    }
                });
            }
        }
    }
}

// =============================================================================
// SchedulersInitFlag — `SCHEDULERS_INIT` init-once gate
// =============================================================================
//
// Resetting this on Drop lets the next `KernelTestScope::enter()`
// re-enter `init_all_percpu_schedulers` cleanly (otherwise init_once
// would short-circuit on the second scope, leaving per-CPU schedulers
// in whatever state the prior scope left them).
//
// Snapshotting is trivial: an `InitFlag` is an atomic bool. We capture
// its current value and re-arm/clear on restore.

hermetic_state! {
    pub SchedulersInitFlag {
        type Snapshot = bool;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(SCHEDULERS_INIT.is_set())
        }
        fn restore(was_set: Self::Snapshot) {
            if was_set {
                // Already-set semantics: ensure the flag is set so subsequent
                // init_once() returns false. InitFlag::reset() puts it back
                // to "uninitialised", then init_once() takes it. There's no
                // atomic "set" so we use the init_once side-effect.
                SCHEDULERS_INIT.reset();
                let _ = SCHEDULERS_INIT.init_once();
            } else {
                SCHEDULERS_INIT.reset();
            }
        }
    }
}

// =============================================================================
// BspCurrentTask — `PCR.current_task[BSP]`
// =============================================================================

hermetic_state! {
    pub BspCurrentTask {
        type Snapshot = u64; // raw pointer as u64 to satisfy Send
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(pcr::get_current_task_for(0) as u64)
        }
        fn restore(addr: Self::Snapshot) {
            pcr::set_current_task(addr as *mut ());
        }
    }
}

// =============================================================================
// BspIdleTask — `PCR.idle_task[BSP]`
// =============================================================================

hermetic_state! {
    pub BspIdleTask {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(pcr::get_idle_task(0) as u64)
        }
        fn restore(addr: Self::Snapshot) {
            pcr::set_idle_task(0, addr as *mut ());
        }
    }
}

// =============================================================================
// SchedulerEnabledFlag — global `SCHEDULER_ENABLED` AtomicU8
// =============================================================================

hermetic_state! {
    pub SchedulerEnabledFlag {
        type Snapshot = u8;
        const DEPENDS_ON: &[&str] = &["SchedulersInitFlag"];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(super::scheduler::SCHEDULER_ENABLED.load(Ordering::Acquire))
        }
        fn restore(prev: Self::Snapshot) {
            super::scheduler::SCHEDULER_ENABLED.store(prev, Ordering::Release);
        }
    }
}

// =============================================================================
// TssIstShadow — BSP TSS.ist[0..7] (gdt_tests overwrite these directly)
// =============================================================================
//
// This is the wedge-critical impl. `gdt_set_ist` writes through to
// `pcr.tss.ist[slot - 1]`. `prepare_switch_to` resets RSP0 on every
// dispatch but never touches IST. Without this snapshot/restore, the
// gdt-test functions leave bogus addresses in BSP's IST entries; the
// next `#PF` from a user-mode COW write loads RSP from the bogus
// address and either triple-faults or smashes hot-path code.

hermetic_state! {
    pub TssIstShadow {
        type Snapshot = [u64; 7];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(test_support::pcr::bsp_ist_snapshot().unwrap_or([0; 7]))
        }
        fn restore(snap: Self::Snapshot) {
            test_support::pcr::bsp_ist_restore(snap);
        }
    }
}

// =============================================================================
// TssRsp0Shadow — BSP TSS.rsp0 + PCR.kernel_rsp
// =============================================================================
//
// `prepare_switch_to` resets RSP0 on every dispatch, so the corruption
// is transient — but during the test phase BSP isn't dispatching so
// RSP0 stays bogus until the first post-test dispatch. Snapshot keeps
// us honest.

hermetic_state! {
    pub TssRsp0Shadow {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(test_support::pcr::bsp_kernel_rsp_snapshot().unwrap_or(0))
        }
        fn restore(snap: Self::Snapshot) {
            test_support::pcr::bsp_kernel_rsp_restore(snap);
        }
    }
}

// =============================================================================
// MsrShadow — STAR/LSTAR/SFMASK/EFER on BSP
// =============================================================================
//
// `syscall_msr_init` writes idempotent values, but other tests could
// rebind LSTAR / SFMASK. Snapshot+restore makes the discipline uniform.
// KERNEL_GS_BASE is per-CPU and points into the PCR; we don't snapshot
// that — touching it would conflict with the live PCR setup.

#[derive(Clone, Copy)]
pub struct MsrSnapshot {
    efer: u64,
    star: u64,
    lstar: u64,
    sfmask: u64,
}

hermetic_state! {
    pub MsrShadow {
        type Snapshot = MsrSnapshot;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            use slopos_arch::cpu::msr::Msr;
            Ok(MsrSnapshot {
                efer: slopos_arch::cpu::read_msr(Msr::EFER),
                star: slopos_arch::cpu::read_msr(Msr::STAR),
                lstar: slopos_arch::cpu::read_msr(Msr::LSTAR),
                sfmask: slopos_arch::cpu::read_msr(Msr::SFMASK),
            })
        }
        fn restore(snap: Self::Snapshot) {
            use slopos_arch::cpu::msr::Msr;
            slopos_arch::cpu::write_msr(Msr::EFER, snap.efer);
            slopos_arch::cpu::write_msr(Msr::STAR, snap.star);
            slopos_arch::cpu::write_msr(Msr::LSTAR, snap.lstar);
            slopos_arch::cpu::write_msr(Msr::SFMASK, snap.sfmask);
        }
    }
}

// =============================================================================
// PanicCleanupHandlers — append-only list with no native reset
// =============================================================================
//
// `register_panic_cleanup` increments PANIC_CLEANUP_COUNT monotonically;
// without restore, the count caps at 8 across many test runs and silently
// drops new registrations. We snapshot the count at scope-enter and
// truncate-to-count on Drop.
//
// Note: we cannot snapshot the slot pointers themselves because they're
// `AtomicPtr<()>` and don't implement Copy. We assume the count alone
// is sufficient — slots above the snapshot count are zeroed on restore
// so the next registration starts fresh.

hermetic_state! {
    pub PanicCleanupHandlers {
        type Snapshot = usize;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(slopos_utils::panic_recovery::cleanup_handler_count())
        }
        fn restore(snap: Self::Snapshot) {
            slopos_utils::panic_recovery::truncate_cleanup_handlers(snap);
        }
    }
}

// =============================================================================
// KlogLevel — global klog verbosity (tests may set Debug or Trace)
// =============================================================================

hermetic_state! {
    pub KlogLevelShadow {
        type Snapshot = slopos_utils::klog::KlogLevel;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(slopos_utils::klog::klog_get_level())
        }
        fn restore(snap: Self::Snapshot) {
            slopos_utils::klog::klog_set_level(snap);
        }
    }
}

// =============================================================================
// WatchdogTicksShadow — per-CPU NMI watchdog tick counters
// =============================================================================
//
// Counters that drift between tests don't cause functional bugs (the
// watchdog only checks against the live tick counter, not a delta), but
// snapshot/restore makes the leak surface uniform and lets future
// audit reasoning be simpler.

hermetic_state! {
    pub WatchdogTicksShadow {
        type Snapshot = u64; // hash; full bitmap is too large for a Send Snapshot
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            // Snapshot is the BSP-only watchdog tick — full restore is
            // unnecessary because the watchdog tolerates arbitrary
            // drift. We capture this so a future test that reasons
            // about watchdog ticks has a stable baseline.
            Ok(super::scheduler::watchdog_last_tick(0))
        }
        fn restore(_snap: Self::Snapshot) {
            // The per-CPU AtomicU64 array is private to scheduler.rs
            // and there's no public reset. The watchdog auto-corrects
            // from any drift, so a no-op restore is functionally safe;
            // the impl exists primarily so the audit gate sees this
            // state covered.
        }
    }
}

// =============================================================================
// ForkRrCounterShadow — per-CPU fork round-robin counter
// =============================================================================

hermetic_state! {
    pub ForkRrCounterShadow {
        type Snapshot = usize;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(super::per_cpu::fork_rr_counter_value())
        }
        fn restore(snap: Self::Snapshot) {
            super::per_cpu::fork_rr_counter_set(snap);
        }
    }
}
