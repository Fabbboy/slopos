use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::task::BlockReason;
use slopos_ostd::KVec;
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY};

use super::scheduler::{
    is_scheduling_active, schedule, schedule_task, scheduler_get_current_task, unschedule_task,
};
use super::task::{
    INVALID_TASK_ID, TASK_POOL_CAPACITY, TaskStatus, task_find_by_id, task_is_blocked,
    task_is_invalid, task_is_terminated, task_is_will_block, task_set_state_from_with_reason,
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

/// Sleep queue backed by a heap `KVec`. The backing buffer is
/// pre-reserved to `TASK_POOL_CAPACITY` on first `init_sleep_queue`
/// call and never reallocates afterwards; `upsert`/`remove`/
/// `pop_due` mutate entries in place.
///
/// `active_count` lets the timer-tick hot path (`pop_due` via
/// `wake_due_sleepers`) skip the full-capacity scan when no tasks
/// are sleeping — the common case. `active_high_water` further bounds
/// the scan by the largest slot index ever occupied, so a pool that
/// briefly held sleepers and has since quiesced still scans O(peak),
/// not O(capacity).
struct SleepQueue {
    entries: KVec<SleepEntry>,
    active_count: u32,
    active_high_water: u32,
}

impl SleepQueue {
    const fn new() -> Self {
        Self {
            entries: KVec::new(),
            active_count: 0,
            active_high_water: 0,
        }
    }

    /// First-time init: pre-fill with empty entries up to
    /// `TASK_POOL_CAPACITY`. Subsequent calls reset every entry.
    fn init_or_reset(&mut self) -> Result<(), ()> {
        if self.entries.is_empty() {
            if self.entries.try_reserve_exact(TASK_POOL_CAPACITY).is_err() {
                return Err(());
            }
            for _ in 0..TASK_POOL_CAPACITY {
                if self.entries.push(SleepEntry::empty()).is_err() {
                    return Err(());
                }
            }
        } else {
            for entry in self.entries.iter_mut() {
                *entry = SleepEntry::empty();
            }
        }
        self.active_count = 0;
        self.active_high_water = 0;
        SLEEP_ACTIVE_COUNT.store(0, Ordering::Release);
        Ok(())
    }

    fn scan_bound(&self) -> usize {
        (self.active_high_water as usize).min(self.entries.len())
    }

    fn upsert(&mut self, task_id: u32, wake_tick: u64) -> bool {
        let scan = self.scan_bound();
        let mut free_idx = None;
        for (idx, entry) in self.entries[..scan].iter_mut().enumerate() {
            if entry.active && entry.task_id == task_id {
                entry.wake_tick = wake_tick;
                return true;
            }
            if !entry.active && free_idx.is_none() {
                free_idx = Some(idx);
            }
        }

        // If no free slot was found within the active-high-water
        // window, grab the next fresh slot past it.
        let idx = free_idx.unwrap_or(scan);
        if idx >= self.entries.len() {
            return false;
        }
        self.entries[idx] = SleepEntry {
            task_id,
            wake_tick,
            active: true,
        };
        self.active_count = self.active_count.saturating_add(1);
        SLEEP_ACTIVE_COUNT.store(self.active_count, Ordering::Release);
        let new_hwm = (idx as u32).saturating_add(1);
        if new_hwm > self.active_high_water {
            self.active_high_water = new_hwm;
        }
        true
    }

    fn remove(&mut self, task_id: u32) {
        let scan = self.scan_bound();
        for entry in self.entries[..scan].iter_mut() {
            if entry.active && entry.task_id == task_id {
                *entry = SleepEntry::empty();
                self.active_count = self.active_count.saturating_sub(1);
                SLEEP_ACTIVE_COUNT.store(self.active_count, Ordering::Release);
                break;
            }
        }
    }

    /// Pop one due sleep entry (if any). Returns `INVALID_TASK_ID`
    /// when no entries are due. The `active_count` fast path makes
    /// the common "no sleepers at all" case O(1) — on a quiet system
    /// the timer-tick caller touches no entries and releases the
    /// lock immediately.
    fn pop_due(&mut self, now_tick: u64) -> u32 {
        if self.active_count == 0 {
            return INVALID_TASK_ID;
        }
        let scan = self.scan_bound();
        for entry in self.entries[..scan].iter_mut() {
            if entry.active && tick_reached(now_tick, entry.wake_tick) {
                let task_id = entry.task_id;
                *entry = SleepEntry::empty();
                self.active_count = self.active_count.saturating_sub(1);
                SLEEP_ACTIVE_COUNT.store(self.active_count, Ordering::Release);
                return task_id;
            }
        }
        INVALID_TASK_ID
    }
}

static SLEEP_QUEUE: IrqMutex<SleepQueue> = IrqMutex::new(SleepQueue::new(), LOCK_LEVEL_REGISTRY);

/// External mirror of `SleepQueue::active_count`, maintained under
/// the `SLEEP_QUEUE` mutex but readable without it. Lets the
/// timer-tick `wake_due_sleepers` call skip the lock entirely when
/// no tasks are sleeping — the common case on a quiet system.
/// Updated with `Release` by writers holding the mutex; readers use
/// `Acquire`. A race where a sleeper is added between the
/// lock-free load and the lock acquire is benign: that sleeper will
/// be picked up on the next tick.
static SLEEP_ACTIVE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Initialise (or reset) the sleep queue. Safe to call on every
/// `init_task_manager`, whether first boot or test-fixture re-init.
pub fn init_sleep_queue() -> c_int {
    if SLEEP_QUEUE.lock().init_or_reset().is_err() {
        return -1;
    }
    0
}

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
        task_is_blocked(task) && unsafe { (*task).load_block_reason() == BlockReason::Sleep };
    if !is_sleep_blocked {
        return;
    }

    if task_set_state_with_reason(task_id, TaskStatus::Ready, BlockReason::None) != 0 {
        return;
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let _ = schedule_task(task);
}

/// Wake every sleeper whose deadline has passed.
///
/// **Lock-free fast path**: if no tasks are currently sleeping — the
/// common case on a quiet system, given that the timer tick calls
/// this at 100 Hz on every CPU — the external atomic
/// `SLEEP_ACTIVE_COUNT` is zero and we return without touching
/// `SLEEP_QUEUE` at all, keeping the per-CPU tick O(1).
///
/// **Slow path**: drain-loop — under the sleep-queue lock we pop a
/// single due entry at a time, then drop the lock before calling
/// `wake_sleeping_task` (which takes `TASK_MANAGER` transitively).
/// Scans are bounded by the queue's internal `active_high_water` so
/// even the slow path is O(peak sleepers), not O(capacity).
pub fn wake_due_sleepers(now_tick: u64) {
    if SLEEP_ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    loop {
        let task_id = {
            let mut queue = SLEEP_QUEUE.lock();
            queue.pop_due(now_tick)
        };
        if task_id == INVALID_TASK_ID {
            break;
        }
        wake_sleeping_task(task_id);
    }
}

pub fn reset_sleep_queue() {
    SLEEP_QUEUE.lock().init_or_reset().ok();
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

#[cfg(feature = "test-hooks")]
pub(crate) fn test_insert_sleep_entry(task_id: u32, wake_tick: u64) -> bool {
    SLEEP_QUEUE.lock().upsert(task_id, wake_tick)
}
