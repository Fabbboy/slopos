//! Periodic Load Balancer for SMP

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::task::Task;
use slopos_lib::{get_cpu_count, klog_debug};

use super::per_cpu::{get_cpu_scheduler, with_cpu_scheduler};
use super::work_steal::{calculate_load_imbalance, find_least_loaded_cpu, find_most_loaded_cpu};

static BALANCE_INTERVAL_MS: AtomicU64 = AtomicU64::new(100);
static LAST_BALANCE_TIME: AtomicU64 = AtomicU64::new(0);
static IMBALANCE_THRESHOLD_PERCENT: AtomicU64 = AtomicU64::new(25);

pub fn set_balance_interval(ms: u64) {
    BALANCE_INTERVAL_MS.store(ms, Ordering::Relaxed);
}

pub fn set_imbalance_threshold(percent: u64) {
    IMBALANCE_THRESHOLD_PERCENT.store(percent.min(100), Ordering::Relaxed);
}

pub fn periodic_load_balance(current_time_ms: u64) {
    let interval = BALANCE_INTERVAL_MS.load(Ordering::Relaxed);
    let last_time = LAST_BALANCE_TIME.load(Ordering::Relaxed);

    if current_time_ms.saturating_sub(last_time) < interval {
        return;
    }

    LAST_BALANCE_TIME.store(current_time_ms, Ordering::Relaxed);

    let cpu_count = get_cpu_count();
    if cpu_count <= 1 {
        return;
    }

    let (min_load, avg_load, max_load) = calculate_load_imbalance();

    if avg_load == 0 {
        return;
    }

    let threshold = IMBALANCE_THRESHOLD_PERCENT.load(Ordering::Relaxed) as u32;
    let upper_threshold = avg_load.saturating_mul(100 + threshold) / 100;
    let lower_threshold = avg_load.saturating_mul(100 - threshold.min(99)) / 100;

    if max_load <= upper_threshold || min_load >= lower_threshold {
        return;
    }

    if let (Some(max_cpu), Some(min_cpu)) =
        (find_most_loaded_cpu(), find_least_loaded_cpu(usize::MAX))
    {
        if max_cpu != min_cpu {
            let _ = migrate_task_between_cpus(max_cpu, min_cpu);
        }
    }
}

fn migrate_task_between_cpus(from_cpu: usize, to_cpu: usize) -> bool {
    // SAFETY: Load balancing runs on a single CPU and schedulers use internal locking.
    let task = {
        let sched = match unsafe { get_cpu_scheduler(from_cpu) } {
            Some(s) => s,
            None => return false,
        };

        if sched.total_ready_count() <= 1 {
            return false;
        }

        match sched.steal_task() {
            Some(t) => t,
            None => return false,
        }
    };

    let affinity = unsafe { (*task).cpu_affinity };
    if affinity != 0 && (affinity & (1 << to_cpu)) == 0 {
        // SAFETY: Same as above - re-enqueue to original CPU.
        if let Some(sched) = unsafe { get_cpu_scheduler(from_cpu) } {
            sched.enqueue_local(task);
        }
        return false;
    }

    unsafe {
        (*task).migration_count += 1;
    }

    with_cpu_scheduler(to_cpu, |sched| {
        sched.enqueue_local(task);
    });

    klog_debug!(
        "LOAD_BALANCE: Migrated task from CPU {} to CPU {}",
        from_cpu,
        to_cpu
    );

    true
}

pub fn trigger_migration(task: *mut Task) -> bool {
    if task.is_null() {
        return false;
    }

    let affinity = unsafe { (*task).cpu_affinity };
    let current_cpu = unsafe { (*task).last_cpu as usize };

    if affinity == 0 || (affinity & (1 << current_cpu)) != 0 {
        return false;
    }

    let cpu_count = get_cpu_count();
    for cpu_id in 0..cpu_count {
        if (affinity & (1 << cpu_id)) != 0 {
            unsafe {
                (*task).migration_count += 1;
            }

            with_cpu_scheduler(cpu_id, |sched| {
                sched.enqueue_local(task);
            });

            klog_debug!(
                "LOAD_BALANCE: Migrated task to CPU {} due to affinity",
                cpu_id
            );
            return true;
        }
    }

    false
}

pub fn get_load_balance_stats() -> LoadBalanceStats {
    let (min_load, avg_load, max_load) = calculate_load_imbalance();
    LoadBalanceStats {
        min_load,
        avg_load,
        max_load,
        last_balance_time: LAST_BALANCE_TIME.load(Ordering::Relaxed),
        balance_interval: BALANCE_INTERVAL_MS.load(Ordering::Relaxed),
    }
}

pub struct LoadBalanceStats {
    pub min_load: u32,
    pub avg_load: u32,
    pub max_load: u32,
    pub last_balance_time: u64,
    pub balance_interval: u64,
}
