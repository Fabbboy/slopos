//! Work Stealing for SMP Load Balancing

use super::task_struct::Task;
use slopos_arch::{get_cpu_count, get_current_cpu};
use slopos_utils::klog_debug;

use super::per_cpu::{
    affinity_allows_cpu, enqueue_task_on_cpu, get_cpu_ready_count, try_steal_task_from_cpu,
    with_local_scheduler,
};

pub fn try_work_steal() -> bool {
    let cpu_id = get_current_cpu();
    let cpu_count = get_cpu_count();

    if cpu_count <= 1 {
        return false;
    }

    let start = (cpu_id + 1) % cpu_count;

    for i in 0..cpu_count {
        let victim = (start + i) % cpu_count;
        if victim == cpu_id {
            continue;
        }

        if let Some(task) = try_steal_from_cpu(victim, cpu_id) {
            with_local_scheduler(|sched| {
                sched.enqueue_local(task);
            });
            klog_debug!("WORK_STEAL: CPU {} stole task from CPU {}", cpu_id, victim);
            return true;
        }
    }

    false
}

fn try_steal_from_cpu(victim: usize, thief: usize) -> Option<*mut Task> {
    let task = try_steal_task_from_cpu(victim)?;

    let affinity = unsafe { (*task).cpu_affinity };
    if !affinity_allows_cpu(affinity, thief) {
        enqueue_task_on_cpu(victim, task);
        return None;
    }

    unsafe {
        (*task).migration_count += 1;
    }

    Some(task)
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
