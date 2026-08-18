//! Work stealing for SMP load balancing, with three anti-ping-pong guards:
//! an imbalance threshold, cache-hot protection, and a per-CPU cooldown
//! between scans. Each is tuned by a constant below.

use core::sync::atomic::{AtomicU64, Ordering};

use super::per_cpu::{
    affinity_allows_cpu, enqueue_task_on_cpu, get_cpu_ready_count, try_steal_task_from_cpu,
    with_cpu_scheduler, with_local_scheduler,
};
use super::task::TaskRef;
use super::task_struct::Task;
use slopos_arch::{get_cpu_count, get_current_cpu};
use slopos_ostd::{kdiag_timestamp, klog_debug};

/// Minimum load difference (in queued tasks) between victim and thief before a
/// steal is considered. At a 1-task difference the steal would immediately
/// create a reverse imbalance → ping-pong.
const IMBALANCE_THRESHOLD: u32 = 2;

/// TSC cycles a task must have been off-CPU before it is eligible for stealing.
/// 0 disables cache-hot protection, since TSC speed varies under QEMU; a
/// production value would be ~1 500 000 cycles (~500 µs at 3 GHz), matching
/// Linux's documented default.
const MIGRATION_COST_CYCLES: u64 = 0;

/// Minimum timer ticks between work-steal scans on a given CPU: with a 10 ms
/// tick period, 3 ticks ≈ 30 ms.
const STEAL_COOLDOWN_TICKS: u64 = 3;

static LAST_STEAL_TICK: [AtomicU64; slopos_arch::MAX_CPUS] =
    [const { AtomicU64::new(0) }; slopos_arch::MAX_CPUS];

/// Attempt to steal a task from another CPU's ready queue; `true` when one was
/// stolen and enqueued locally.
pub fn try_work_steal() -> bool {
    let cpu_id = get_current_cpu();
    let cpu_count = get_cpu_count();

    if cpu_count <= 1 {
        return false;
    }

    if !steal_cooldown_elapsed(cpu_id) {
        return false;
    }

    let thief_load = get_cpu_load(cpu_id);
    let start = (cpu_id + 1) % cpu_count;

    for i in 0..cpu_count {
        let victim = (start + i) % cpu_count;
        if victim == cpu_id {
            continue;
        }

        let victim_load = get_cpu_load(victim);
        if victim_load < thief_load + IMBALANCE_THRESHOLD {
            continue;
        }

        if let Some(task) = try_steal_from_cpu(victim, cpu_id) {
            // The stolen handle is the task's owning reference for the whole
            // migration window.
            match with_local_scheduler(|sched| sched.enqueue_migrated(task)) {
                Ok(()) => {
                    klog_debug!("WORK_STEAL: CPU {} stole task from CPU {}", cpu_id, victim);
                    return true;
                }
                Err(task) => return_to_victim(victim, task),
            }
        }
    }

    false
}

fn steal_cooldown_elapsed(cpu_id: usize) -> bool {
    if STEAL_COOLDOWN_TICKS == 0 {
        return true;
    }
    let now = with_cpu_scheduler(cpu_id, |s| s.total_ticks.load(Ordering::Relaxed)).unwrap_or(0);
    let last = LAST_STEAL_TICK[cpu_id].load(Ordering::Relaxed);
    if now.saturating_sub(last) < STEAL_COOLDOWN_TICKS {
        return false;
    }
    LAST_STEAL_TICK[cpu_id].store(now, Ordering::Relaxed);
    true
}

fn try_steal_from_cpu(victim: usize, thief: usize) -> Option<TaskRef> {
    let stolen = try_steal_task_from_cpu(victim)?;
    let task: &Task = &stolen;

    let affinity = task.cpu_affinity();
    if !affinity_allows_cpu(affinity, thief) {
        return_to_victim(victim, stolen);
        return None;
    }

    // Assumes an invariant TSC: timestamps taken on different cores are
    // compared directly, which is inaccurate on a CPU without one.
    if MIGRATION_COST_CYCLES > 0 {
        let last_run = task.last_run_timestamp();
        if last_run != 0 {
            let now = kdiag_timestamp();
            if now.saturating_sub(last_run) < MIGRATION_COST_CYCLES {
                return_to_victim(victim, stolen);
                return None;
            }
        }
    }

    // Atomic because this CPU is the thief, not the runner.
    task.inc_migration_count();
    Some(stolen)
}

/// Hand a stolen task back to the CPU it came from, releasing the carried
/// reference once that queue has parked its own. A victim that refuses it still
/// releases the reference; the task keeps its other owners and the rescue sweep
/// re-publishes it.
fn return_to_victim(victim: usize, task: TaskRef) {
    let _ = enqueue_task_on_cpu(victim, &task);
    crate::task::task_put(task);
}

pub fn get_cpu_load(cpu_id: usize) -> u32 {
    get_cpu_ready_count(cpu_id)
}

pub fn find_least_loaded_cpu(exclude: usize) -> Option<usize> {
    let cpu_count = get_cpu_count();
    let mut best_cpu = None;
    let mut min_load = u32::MAX;

    for cpu_id in 0..cpu_count {
        if cpu_id == exclude {
            continue;
        }
        let load = get_cpu_load(cpu_id);
        if load < min_load {
            min_load = load;
            best_cpu = Some(cpu_id);
        }
    }
    best_cpu
}

pub fn find_most_loaded_cpu() -> Option<usize> {
    let cpu_count = get_cpu_count();
    let mut best_cpu = None;
    let mut max_load = 0u32;

    for cpu_id in 0..cpu_count {
        let load = get_cpu_load(cpu_id);
        if load > max_load {
            max_load = load;
            best_cpu = Some(cpu_id);
        }
    }
    best_cpu
}

pub fn calculate_load_imbalance() -> (u32, u32, u32) {
    let cpu_count = get_cpu_count();
    if cpu_count == 0 {
        return (0, 0, 0);
    }

    let mut total_load = 0u32;
    let mut min_load = u32::MAX;
    let mut max_load = 0u32;

    for cpu_id in 0..cpu_count {
        let load = get_cpu_load(cpu_id);
        total_load += load;
        min_load = min_load.min(load);
        max_load = max_load.max(load);
    }

    let avg_load = total_load / cpu_count as u32;
    (min_load, avg_load, max_load)
}
