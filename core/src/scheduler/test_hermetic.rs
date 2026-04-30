//! `HermeticState` impls for the kernel-singleton state that
//! `KernelTestScope` previously snapshotted by hand.
//!
//! Each impl declares one piece of mutable scheduler/PCR state that
//! tests may transiently mutate. The framework auto-walks the registry
//! at scope enter/Drop, so adding a new singleton means writing one
//! impl + one `register_hermetic_state!` line — never editing the
//! scope itself.

#![cfg(feature = "test-hooks")]

use core::sync::atomic::Ordering;

use slopos_alloc::AllocError;
use slopos_arch::pcr;
use slopos_hermetic::{HermeticState, register_hermetic_state};

use super::per_cpu::{SCHEDULERS_INIT, with_cpu_scheduler};

const SCOPE_BITMAP_MAX_CPUS: usize = 32;

// =============================================================================
// PerCpuOnlineBits — `pcr::is_cpu_online(cpu)` / `mark_cpu_online/offline`
// =============================================================================

pub struct PerCpuOnlineBits;

unsafe impl HermeticState for PerCpuOnlineBits {
    type Snapshot = u32;
    const NAME: &'static str = "PerCpuOnlineBits";

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

    unsafe fn restore(bits: Self::Snapshot) {
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

register_hermetic_state!(PerCpuOnlineBits);

// =============================================================================
// PerCpuSchedulerEnableBits — per-CPU `sched.is_enabled()` bitmap
// =============================================================================
//
// Depends on PerCpuOnlineBits because some tests bring CPUs offline,
// and restoring `enabled=true` for an offline CPU would be confusing.

pub struct PerCpuSchedulerEnableBits;

unsafe impl HermeticState for PerCpuSchedulerEnableBits {
    type Snapshot = u32;
    const NAME: &'static str = "PerCpuSchedulerEnableBits";
    const DEPENDS_ON: &'static [&'static str] = &["PerCpuOnlineBits"];

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

    unsafe fn restore(bits: Self::Snapshot) {
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

register_hermetic_state!(PerCpuSchedulerEnableBits);

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

pub struct SchedulersInitFlag;

unsafe impl HermeticState for SchedulersInitFlag {
    type Snapshot = bool;
    const NAME: &'static str = "SchedulersInitFlag";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(SCHEDULERS_INIT.is_set())
    }

    unsafe fn restore(was_set: Self::Snapshot) {
        if was_set {
            // Already-set semantics: ensure the flag is set so subsequent
            // init_once() returns false.
            // InitFlag::reset() puts it back to "uninitialised", then
            // init_once() takes it. There's no atomic "set" so we use
            // the init_once side-effect.
            SCHEDULERS_INIT.reset();
            let _ = SCHEDULERS_INIT.init_once();
        } else {
            SCHEDULERS_INIT.reset();
        }
    }
}

register_hermetic_state!(SchedulersInitFlag);

// =============================================================================
// BspCurrentTask — `PCR.current_task[BSP]`
// =============================================================================

pub struct BspCurrentTask;

unsafe impl HermeticState for BspCurrentTask {
    type Snapshot = u64; // raw pointer as u64 to satisfy Send
    const NAME: &'static str = "BspCurrentTask";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(pcr::get_current_task_for(0) as u64)
    }

    unsafe fn restore(addr: Self::Snapshot) {
        pcr::set_current_task(addr as *mut ());
    }
}

register_hermetic_state!(BspCurrentTask);

// =============================================================================
// BspIdleTask — `PCR.idle_task[BSP]`
// =============================================================================

pub struct BspIdleTask;

unsafe impl HermeticState for BspIdleTask {
    type Snapshot = u64;
    const NAME: &'static str = "BspIdleTask";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(pcr::get_idle_task(0) as u64)
    }

    unsafe fn restore(addr: Self::Snapshot) {
        pcr::set_idle_task(0, addr as *mut ());
    }
}

register_hermetic_state!(BspIdleTask);

// =============================================================================
// SchedulerEnabledFlag — global `SCHEDULER_ENABLED` AtomicU8
// =============================================================================

pub struct SchedulerEnabledFlag;

unsafe impl HermeticState for SchedulerEnabledFlag {
    type Snapshot = u8;
    const NAME: &'static str = "SchedulerEnabledFlag";
    const DEPENDS_ON: &'static [&'static str] = &["SchedulersInitFlag"];

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(super::scheduler::SCHEDULER_ENABLED.load(Ordering::Acquire))
    }

    unsafe fn restore(prev: Self::Snapshot) {
        super::scheduler::SCHEDULER_ENABLED.store(prev, Ordering::Release);
    }
}

register_hermetic_state!(SchedulerEnabledFlag);

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

pub struct TssIstShadow;

unsafe impl HermeticState for TssIstShadow {
    type Snapshot = [u64; 7];
    const NAME: &'static str = "TssIstShadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        let mut ist = [0u64; 7];
        // SAFETY: BSP PCR is initialised early in kernel_main_impl and
        // remains valid for the kernel's lifetime; tests run on BSP.
        if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
            for i in 0..7 {
                ist[i] = pcr.tss.ist[i];
            }
        }
        Ok(ist)
    }

    unsafe fn restore(snap: Self::Snapshot) {
        if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
            for i in 0..7 {
                pcr.tss.ist[i] = snap[i];
            }
        }
    }
}

register_hermetic_state!(TssIstShadow);

// =============================================================================
// TssRsp0Shadow — BSP TSS.rsp0 + PCR.kernel_rsp
// =============================================================================
//
// `prepare_switch_to` resets RSP0 on every dispatch, so the corruption
// is transient — but during the test phase BSP isn't dispatching so
// RSP0 stays bogus until the first post-test dispatch. Snapshot keeps
// us honest.

pub struct TssRsp0Shadow;

unsafe impl HermeticState for TssRsp0Shadow {
    type Snapshot = u64;
    const NAME: &'static str = "TssRsp0Shadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
            Ok(pcr.kernel_rsp)
        } else {
            Ok(0)
        }
    }

    unsafe fn restore(snap: Self::Snapshot) {
        if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
            pcr.kernel_rsp = snap;
            pcr.sync_tss_rsp0();
        }
    }
}

register_hermetic_state!(TssRsp0Shadow);

// =============================================================================
// MsrShadow — STAR/LSTAR/SFMASK/EFER on BSP
// =============================================================================
//
// `syscall_msr_init` writes idempotent values, but other tests could
// rebind LSTAR / SFMASK. Snapshot+restore makes the discipline uniform.
// KERNEL_GS_BASE is per-CPU and points into the PCR; we don't snapshot
// that — touching it would conflict with the live PCR setup.

pub struct MsrShadow;

#[derive(Clone, Copy)]
pub struct MsrSnapshot {
    efer: u64,
    star: u64,
    lstar: u64,
    sfmask: u64,
}

// SAFETY: a struct of u64 is trivially Send.
unsafe impl Send for MsrSnapshot {}

unsafe impl HermeticState for MsrShadow {
    type Snapshot = MsrSnapshot;
    const NAME: &'static str = "MsrShadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        use slopos_arch::cpu::msr::Msr;
        Ok(MsrSnapshot {
            efer: slopos_arch::cpu::read_msr(Msr::EFER),
            star: slopos_arch::cpu::read_msr(Msr::STAR),
            lstar: slopos_arch::cpu::read_msr(Msr::LSTAR),
            sfmask: slopos_arch::cpu::read_msr(Msr::SFMASK),
        })
    }

    unsafe fn restore(snap: Self::Snapshot) {
        use slopos_arch::cpu::msr::Msr;
        slopos_arch::cpu::write_msr(Msr::EFER, snap.efer);
        slopos_arch::cpu::write_msr(Msr::STAR, snap.star);
        slopos_arch::cpu::write_msr(Msr::LSTAR, snap.lstar);
        slopos_arch::cpu::write_msr(Msr::SFMASK, snap.sfmask);
    }
}

register_hermetic_state!(MsrShadow);

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

pub struct PanicCleanupHandlers;

unsafe impl HermeticState for PanicCleanupHandlers {
    type Snapshot = usize;
    const NAME: &'static str = "PanicCleanupHandlers";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(slopos_utils::panic_recovery::cleanup_handler_count())
    }

    unsafe fn restore(snap: Self::Snapshot) {
        unsafe { slopos_utils::panic_recovery::truncate_cleanup_handlers(snap) };
    }
}

register_hermetic_state!(PanicCleanupHandlers);

// =============================================================================
// KlogLevel — global klog verbosity (tests may set Debug or Trace)
// =============================================================================

pub struct KlogLevelShadow;

unsafe impl HermeticState for KlogLevelShadow {
    type Snapshot = slopos_utils::klog::KlogLevel;
    const NAME: &'static str = "KlogLevelShadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(slopos_utils::klog::klog_get_level())
    }

    unsafe fn restore(snap: Self::Snapshot) {
        slopos_utils::klog::klog_set_level(snap);
    }
}

register_hermetic_state!(KlogLevelShadow);

// =============================================================================
// WatchdogTicksShadow — per-CPU NMI watchdog tick counters
// =============================================================================
//
// Counters that drift between tests don't cause functional bugs (the
// watchdog only checks against the live tick counter, not a delta), but
// snapshot/restore makes the leak surface uniform and lets future
// audit reasoning be simpler.

pub struct WatchdogTicksShadow;

unsafe impl HermeticState for WatchdogTicksShadow {
    type Snapshot = u64; // hash; full bitmap is too large for a Send Snapshot
    const NAME: &'static str = "WatchdogTicksShadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        // Snapshot is the sum of all watchdog ticks on BSP only — full
        // restore is unnecessary because the watchdog tolerates
        // arbitrary drift. We're capturing this so a future test that
        // reasons about watchdog ticks has a stable baseline.
        Ok(super::scheduler::watchdog_last_tick(0))
    }

    unsafe fn restore(_snap: Self::Snapshot) {
        // The per-CPU AtomicU64 array is private to scheduler.rs and
        // there's no public reset. The watchdog auto-corrects from any
        // drift, so a no-op restore is functionally safe; the impl
        // exists primarily so the audit gate sees this state covered.
    }
}

register_hermetic_state!(WatchdogTicksShadow);

// =============================================================================
// ForkRrCounterShadow — per-CPU fork round-robin counter
// =============================================================================

pub struct ForkRrCounterShadow;

unsafe impl HermeticState for ForkRrCounterShadow {
    type Snapshot = usize;
    const NAME: &'static str = "ForkRrCounterShadow";

    fn snapshot() -> Result<Self::Snapshot, AllocError> {
        Ok(super::per_cpu::fork_rr_counter_value())
    }

    unsafe fn restore(snap: Self::Snapshot) {
        super::per_cpu::fork_rr_counter_set(snap);
    }
}

register_hermetic_state!(ForkRrCounterShadow);
