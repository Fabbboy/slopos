//! `HermeticState` impls for the scheduler/PCR singletons a test may
//! transiently mutate.
//!
//! One `hermetic_state! { ... }` block per singleton emits the marker struct,
//! the trait impl, and the `.hermetic_state_registry` entry the framework
//! auto-walks at scope enter/Drop, so adding one never means editing the scope.

#![cfg(feature = "test-hooks")]

use core::sync::atomic::Ordering;

use slopos_arch::pcr;
use slopos_ostd::cpu::x86_64::interrupts::IrqDisabled;
use slopos_ostd::hermetic_state;
use slopos_ostd::test_support;

use super::per_cpu::{SCHEDULERS_INIT, with_cpu_scheduler};

const SCOPE_BITMAP_MAX_CPUS: usize = 32;

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

// Ordered after PerCpuOnlineBits: some tests bring CPUs offline, and restoring
// `enabled = true` for an offline CPU would be confusing.
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

// Without the Drop reset, `init_once` short-circuits on the second scope and
// leaves the per-CPU schedulers in whatever state the prior scope left them.
hermetic_state! {
    pub SchedulersInitFlag {
        type Snapshot = bool;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(SCHEDULERS_INIT.is_set())
        }
        fn restore(was_set: Self::Snapshot) {
            if was_set {
                // There is no atomic "set": reset then init_once is how the
                // flag is re-armed.
                SCHEDULERS_INIT.reset();
                let _ = SCHEDULERS_INIT.init_once();
            } else {
                SCHEDULERS_INIT.reset();
            }
        }
    }
}

hermetic_state! {
    pub BspCurrentTask {
        // Pointer as u64 to satisfy Send, paired with the id and priority the
        // PCR published for it: restoring the pointer alone would leave the
        // other two describing a different task.
        type Snapshot = (u64, u32, u8);
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok((
                pcr::get_current_task_for(0) as u64,
                pcr::current_task_id_for(0),
                pcr::current_task_priority_for(0),
            ))
        }
        fn restore(saved: Self::Snapshot) {
            let (addr, task_id, priority) = saved;
            // `INVALID_TASK_ID` is stamped whenever the slot does not name a
            // heap task, so the id decides which publisher to restore through.
            if task_id == slopos_abi::task::INVALID_TASK_ID {
                pcr::park_bootstrap_task(addr as *mut ());
            } else {
                pcr::set_current_task_typed(addr as *mut crate::task_struct::Task, task_id, priority);
            }
        }
    }
}

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

// `prepare_switch_to` resets RSP0 on every dispatch but never touches IST, so
// a bogus IST entry left by a gdt test survives until the next `#PF` loads RSP
// from it and either triple-faults or smashes hot-path code.
hermetic_state! {
    pub TssIstShadow {
        type Snapshot = [u64; 7];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(IrqDisabled::with(|irq| test_support::pcr::bsp_ist_snapshot(irq)).unwrap_or([0; 7]))
        }
        fn restore(snap: Self::Snapshot) {
            IrqDisabled::with(|irq| test_support::pcr::bsp_ist_restore(irq, snap));
        }
    }
}

// RSP0 corruption self-heals on the next dispatch, but BSP does not dispatch
// during the test phase.
hermetic_state! {
    pub TssRsp0Shadow {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(IrqDisabled::with(|irq| test_support::pcr::bsp_kernel_rsp_snapshot(irq)).unwrap_or(0))
        }
        fn restore(snap: Self::Snapshot) {
            IrqDisabled::with(|irq| test_support::pcr::bsp_kernel_rsp_restore(irq, snap));
        }
    }
}

// KERNEL_GS_BASE is deliberately excluded: it points into the live PCR.
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

// Registration is monotonic and caps at 8, so without a truncate-to-count
// restore a long run silently drops new registrations. The slot pointers are
// `AtomicPtr<()>` and not Copy; slots above the count are zeroed on restore.
hermetic_state! {
    pub PanicCleanupHandlers {
        type Snapshot = usize;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(slopos_ostd::panic_recovery::cleanup_handler_count())
        }
        fn restore(snap: Self::Snapshot) {
            slopos_ostd::panic_recovery::truncate_cleanup_handlers(snap);
        }
    }
}

hermetic_state! {
    pub OopsLedgerShadow {
        type Snapshot = (u64, u64);
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok((
                slopos_ostd::panic_recovery::oops_count(),
                slopos_ostd::panic_recovery::oops_limit(),
            ))
        }
        fn restore(snap: Self::Snapshot) {
            slopos_ostd::panic_recovery::restore_oops_ledger(snap.0, snap.1);
        }
    }
}

hermetic_state! {
    pub KlogLevelShadow {
        type Snapshot = slopos_ostd::klog::KlogLevel;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(slopos_ostd::klog::klog_get_level())
        }
        fn restore(snap: Self::Snapshot) {
            slopos_ostd::klog::klog_set_level(snap);
        }
    }
}

hermetic_state! {
    pub WatchdogHeartbeatShadow {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(slopos_arch::pcr::heartbeat_for_cpu(0))
        }
        fn restore(_snap: Self::Snapshot) {
            // Monotonic, and only ever read as a difference against the
            // reader's own last sample; captured so the audit gate sees it.
        }
    }
}

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
