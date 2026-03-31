use core::ffi::c_int;

use slopos_abi::task::{BlockReason, MAX_TASKS};
use slopos_sync::IrqMutex;

use super::scheduler::{
    is_scheduling_active, schedule, schedule_task, scheduler_get_current_task, unschedule_task,
};
use super::task::{
    INVALID_TASK_ID, TaskStatus, task_find_by_id, task_is_blocked, task_is_invalid,
    task_is_terminated, task_is_will_block, task_set_state_from_with_reason,
    task_set_state_with_reason,
};
use slopos_kernel_services::platform;

#[derive(Copy, Clone)]
struct SleepEntry {
    task_id: u32,
    wake_tick: u64,
    active: bool,
}

impl SleepEntry {
    const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            wake_tick: 0,
            active: false,
        }
    }
}

struct SleepQueue {
    entries: [SleepEntry; MAX_TASKS],
}

impl SleepQueue {
    const fn new() -> Self {
        Self {
            entries: [SleepEntry::empty(); MAX_TASKS],
        }
    }

    fn clear(&mut self) {
        self.entries = [SleepEntry::empty(); MAX_TASKS];
    }

    fn upsert(&mut self, task_id: u32, wake_tick: u64) -> bool {
        let mut free_idx = None;
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            if entry.active && entry.task_id == task_id {
                entry.wake_tick = wake_tick;
                return true;
            }
            if !entry.active && free_idx.is_none() {
                free_idx = Some(idx);
            }
        }

        if let Some(idx) = free_idx {
            self.entries[idx] = SleepEntry {
                task_id,
                wake_tick,
                active: true,
            };
            true
        } else {
            false
        }
    }

    fn remove(&mut self, task_id: u32) {
        for entry in self.entries.iter_mut() {
            if entry.active && entry.task_id == task_id {
                *entry = SleepEntry::empty();
                break;
            }
        }
    }

    fn collect_due(&mut self, now_tick: u64, out: &mut [u32; MAX_TASKS]) -> usize {
        let mut count = 0usize;
        for entry in self.entries.iter_mut() {
            if !entry.active {
                continue;
            }
            if tick_reached(now_tick, entry.wake_tick) {
                if count < out.len() {
                    out[count] = entry.task_id;
                    count += 1;
                }
                *entry = SleepEntry::empty();
            }
        }
        count
    }
}

static SLEEP_QUEUE: IrqMutex<SleepQueue> = IrqMutex::new(SleepQueue::new());

#[inline]
fn tick_reached(now_tick: u64, deadline_tick: u64) -> bool {
    now_tick.wrapping_sub(deadline_tick) < (1u64 << 63)
}

fn ms_to_sleep_ticks(ms: u32) -> u64 {
    let freq = platform::timer_frequency() as u64;
    if freq == 0 {
        return 1;
    }

    let ticks = (ms as u64).saturating_mul(freq).saturating_add(999) / 1000;
    ticks.max(1)
}

fn wake_sleeping_task(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }

    let task = task_find_by_id(task_id);
    if task.is_null() || task_is_invalid(task) || task_is_terminated(task) {
        return;
    }

    let is_sleep_blocked =
        task_is_blocked(task) && unsafe { (*task).block_reason == BlockReason::Sleep };
    if !is_sleep_blocked {
        return;
    }

    if task_set_state_with_reason(task_id, TaskStatus::Ready, BlockReason::None) != 0 {
        return;
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let _ = schedule_task(task);
}

pub fn wake_due_sleepers(now_tick: u64) {
    let mut due = [INVALID_TASK_ID; MAX_TASKS];
    let due_count = {
        let mut queue = SLEEP_QUEUE.lock();
        queue.collect_due(now_tick, &mut due)
    };

    for task_id in due.iter().take(due_count) {
        wake_sleeping_task(*task_id);
    }
}

pub fn reset_sleep_queue() {
    SLEEP_QUEUE.lock().clear();
}

#[cfg(feature = "itests")]
pub(crate) fn test_insert_sleep_entry(task_id: u32, wake_tick: u64) -> bool {
    SLEEP_QUEUE.lock().upsert(task_id, wake_tick)
}

pub fn cancel_sleep(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    SLEEP_QUEUE.lock().remove(task_id);
}

pub fn sleep_current_task_ms(ms: u32) -> c_int {
    if ms == 0 {
        return 0;
    }

    if !is_scheduling_active() {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let current = scheduler_get_current_task();
    if current.is_null() {
        return -1;
    }
    if super::per_cpu::is_idle_task(current) {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let task_id = unsafe { (*current).task_id };
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(ms));

    {
        let mut queue = SLEEP_QUEUE.lock();
        if !queue.upsert(task_id, wake_tick) {
            return -1;
        }
        // Only block from Running state. WillBlock-aware callers must use
        // block_current_task_with_timeout instead.
        if task_set_state_from_with_reason(
            task_id,
            TaskStatus::Running,
            TaskStatus::Blocked,
            BlockReason::Sleep,
        ) != 0
        {
            queue.remove(task_id);
            return -1;
        }
        unschedule_task(current);
    }
    schedule();
    0
}

/// Block the current task with a timeout.
///
/// Combines scheduler-backed blocking (`block_current_task` semantics)
/// with sleep-queue timeout (`sleep_current_task_ms` semantics).
///
/// The caller is expected to have registered a wakeup mechanism (e.g.
/// storing the task handle so an IRQ can call `unblock_task`). This
/// function provides the timeout safety net: if the external signal
/// never arrives, the sleep timer wakes us after `timeout_ms`.
///
/// # Safety contract
/// Must NOT be called while holding an `IrqMutex` — blocking with a
/// mutex held risks deadlock if another task contends the same lock.
pub fn block_current_task_with_timeout(timeout_ms: u32) {
    if !is_scheduling_active() {
        // No scheduler — fall through; the caller's spin fallback handles it.
        return;
    }

    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if super::per_cpu::is_idle_task(current) {
        return;
    }

    let task_id = unsafe { (*current).task_id };
    if task_id == INVALID_TASK_ID {
        return;
    }

    // Fast-path: if wakeup already arrived (Running), skip entirely.
    // This is an optimization only — the CAS below is the authority.
    if !task_is_will_block(current) {
        return;
    }

    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(timeout_ms));
    if !SLEEP_QUEUE.lock().upsert(task_id, wake_tick) {
        return;
    }

    // Atomic CAS(WillBlock, Blocked): only blocks if still in WillBlock.
    // If a concurrent unblock_task set Running between the fast-path check
    // and here, the CAS fails — the wakeup is preserved.
    if task_set_state_from_with_reason(
        task_id,
        TaskStatus::WillBlock,
        TaskStatus::Blocked,
        BlockReason::Sleep,
    ) != 0
    {
        cancel_sleep(task_id);
        return;
    }

    unschedule_task(current);
    schedule();
    cancel_sleep(task_id);
}
