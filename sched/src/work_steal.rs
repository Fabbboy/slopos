//! SMP load balancing: an idle CPU pulls work, and a loaded CPU pushes it.
//!
//! Pull alone cannot correct an imbalance in which no CPU is idle, because
//! nobody reaches the idle loop to scan. [`periodic_balance`] closes that case.

use core::sync::atomic::{AtomicU64, Ordering};

use super::per_cpu::{
    affinity_allows_cpu, enqueue_task_on_cpu, is_schedulable_cpu, try_steal_task_from_cpu,
    with_cpu_scheduler, with_local_scheduler,
};
use super::task::TaskRef;
use super::task_struct::Task;
use slopos_arch::{get_cpu_count, get_current_cpu};
use slopos_ostd::kdiag_timestamp;

/// Minimum load difference between victim and thief before a task is moved. At
/// a 1-task difference the move creates a reverse imbalance → ping-pong.
const IMBALANCE_THRESHOLD: u32 = 2;

/// TSC cycles a task must have been off-CPU before it is eligible for stealing.
/// 0 disables cache-hot protection, since TSC speed varies under QEMU; a
/// production value would be ~1 500 000 cycles (~500 µs at 3 GHz), matching
/// Linux's documented default.
const MIGRATION_COST_CYCLES: u64 = 0;

/// Minimum timer ticks between *proactive* work-steal scans on a given CPU:
/// with a 10 ms tick period, 3 ticks ≈ 30 ms.
const STEAL_COOLDOWN_TICKS: u64 = 3;

/// Timer ticks between push-balance passes.
const BALANCE_INTERVAL_TICKS: u64 = 8;

static LAST_STEAL_TICK: [AtomicU64; slopos_arch::MAX_CPUS] =
    [const { AtomicU64::new(0) }; slopos_arch::MAX_CPUS];

static STEALS: [AtomicU64; slopos_arch::MAX_CPUS] =
    [const { AtomicU64::new(0) }; slopos_arch::MAX_CPUS];
static PUSHES: [AtomicU64; slopos_arch::MAX_CPUS] =
    [const { AtomicU64::new(0) }; slopos_arch::MAX_CPUS];

/// Tasks `cpu_id` has pulled from a peer and pushed to one.
pub fn migration_counts(cpu_id: usize) -> (u64, u64) {
    if cpu_id >= slopos_arch::MAX_CPUS {
        return (0, 0);
    }
    (
        STEALS[cpu_id].load(Ordering::Relaxed),
        PUSHES[cpu_id].load(Ordering::Relaxed),
    )
}

/// Attempt to steal a task from another CPU's ready queue; `true` when one was
/// stolen and enqueued locally.
pub fn try_work_steal() -> bool {
    let cpu_id = get_current_cpu();
    let cpu_count = get_cpu_count();

    if cpu_count <= 1 {
        return false;
    }

    let thief_load = get_cpu_load(cpu_id);

    // The cooldown bounds *proactive* scanning; an idle CPU has no work to
    // take cycles from.
    if thief_load != 0 && !steal_cooldown_elapsed(cpu_id) {
        return false;
    }

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
                    STEALS[cpu_id].fetch_add(1, Ordering::Relaxed);
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

/// Runnable tasks on `cpu_id`, counting the one currently running.
///
/// Queued-only reads 1-running-plus-1-queued as load 1, which never reaches
/// [`IMBALANCE_THRESHOLD`] against an idle CPU's 0.
pub fn get_cpu_load(cpu_id: usize) -> u32 {
    with_cpu_scheduler(cpu_id, |sched| sched.effective_load()).unwrap_or(0)
}

/// Push one queued task from this CPU to the least-loaded eligible peer, and
/// report whether one moved.
///
/// Driven from the timer tick because `try_work_steal` runs only from the idle
/// loop: with every CPU holding at least one task, nobody is there to pull.
pub fn periodic_balance() -> bool {
    let cpu_id = get_current_cpu();
    let cpu_count = get_cpu_count();
    if cpu_count <= 1 {
        return false;
    }

    if !balance_interval_elapsed(cpu_id) {
        return false;
    }

    let my_load = get_cpu_load(cpu_id);
    if my_load < IMBALANCE_THRESHOLD {
        return false;
    }

    let Some((target, target_load)) = least_loaded_peer(cpu_id) else {
        return false;
    };
    if my_load < target_load.saturating_add(IMBALANCE_THRESHOLD) {
        return false;
    }

    let Some(task) = try_steal_task_from_cpu(cpu_id) else {
        return false;
    };

    if !affinity_allows_cpu(task.cpu_affinity(), target) {
        return_to_victim(cpu_id, task);
        return false;
    }

    task.inc_migration_count();
    if enqueue_task_on_cpu(target, &task) != 0 {
        return_to_victim(cpu_id, task);
        return false;
    }
    crate::task::task_put(task);
    crate::lifecycle::send_reschedule_ipi(target);
    PUSHES[cpu_id].fetch_add(1, Ordering::Relaxed);
    true
}

/// Phase-shifted by `cpu_id` so the CPUs do not all scan on the same tick.
fn balance_interval_elapsed(cpu_id: usize) -> bool {
    let ticks = with_cpu_scheduler(cpu_id, |s| s.total_ticks.load(Ordering::Relaxed)).unwrap_or(0);
    ticks.wrapping_add(cpu_id as u64) % BALANCE_INTERVAL_TICKS == 0
}

fn least_loaded_peer(exclude: usize) -> Option<(usize, u32)> {
    let cpu_count = get_cpu_count();
    let mut best: Option<(usize, u32)> = None;
    for cpu_id in 0..cpu_count {
        if cpu_id == exclude || !is_schedulable_cpu(cpu_id, 0) {
            continue;
        }
        let load = get_cpu_load(cpu_id);
        if best.is_none_or(|(_, best_load)| load < best_load) {
            best = Some((cpu_id, load));
        }
    }
    best
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
