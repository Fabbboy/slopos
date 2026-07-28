use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::task::BlockReason;
use slopos_ostd::KVec;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::scheduler::{
    commit_blocked_deschedule, consume_ready_wake_for_current, is_scheduling_active, schedule,
    wake_blocked_task,
};
use super::task::{
    INVALID_TASK_ID, MAX_TASKS, TaskStatus, task_find_by_id, task_set_state_from_with_reason,
};
use slopos_kernel_services::platform;

#[derive(Copy, Clone)]
struct SleepEntry {
    task_id: u32,
    wake_tick: u64,
    /// Arm generation, bumped on every `upsert`. Removal by the timer path
    /// is generation-checked so a wake delivered against one park can never
    /// delete the entry of a later park (the owner may have re-armed on
    /// another CPU in the meantime) — the cross-generation cancel was the
    /// `net-timer` strand.
    generation: u64,
    /// Consecutive due-ticks on which the owner was observed not
    /// sleep-parked. A mid-park transient lasts microseconds, so a large
    /// miss count can only mean a stale entry (owner was event-woken and
    /// never re-armed); it is then scrubbed to stop per-tick churn.
    misses: u8,
    active: bool,
}

impl SleepEntry {
    const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            wake_tick: 0,
            generation: 0,
            misses: 0,
            active: false,
        }
    }
}

/// Sleep queue backed by a heap `KVec`. The backing buffer is
/// pre-reserved to `MAX_TASKS` on first `init_sleep_queue`
/// call and never reallocates afterwards; `upsert`/`remove`/
/// `pop_due` mutate entries in place.
///
/// `active_count` lets the timer-tick hot path (`pop_due` via
/// `wake_due_sleepers`) skip the full-capacity scan when no tasks
/// are sleeping — the common case. `active_high_water` further bounds
/// the scan by the largest slot index ever occupied, so a queue that
/// briefly held sleepers and has since quiesced still scans O(peak),
/// not O(capacity).
struct SleepQueue {
    entries: KVec<SleepEntry>,
    active_count: u32,
    active_high_water: u32,
    /// Monotonic arm-generation source (see [`SleepEntry::generation`]).
    generation_counter: u64,
}

impl SleepQueue {
    const fn new() -> Self {
        Self {
            entries: KVec::new(),
            active_count: 0,
            active_high_water: 0,
            generation_counter: 1,
        }
    }

    /// First-time init: pre-fill with empty entries up to
    /// `MAX_TASKS`. Subsequent calls reset every entry.
    fn init_or_reset(&mut self) -> Result<(), ()> {
        if self.entries.is_empty() {
            if self.entries.try_reserve_exact(MAX_TASKS).is_err() {
                return Err(());
            }
            for _ in 0..MAX_TASKS {
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
        let generation = self.generation_counter;
        self.generation_counter = self.generation_counter.wrapping_add(1);
        let scan = self.scan_bound();
        let mut free_idx = None;
        for (idx, entry) in self.entries[..scan].iter_mut().enumerate() {
            if entry.active && entry.task_id == task_id {
                entry.wake_tick = wake_tick;
                entry.generation = generation;
                entry.misses = 0;
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
            generation,
            misses: 0,
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

    /// Collect the task ids of due sleep entries into `out` WITHOUT
    /// removing them. Entries are only removed once a wake conclusively
    /// publishes `Ready` (`wake_blocked_task`'s success paths call
    /// `cancel_sleep`) or the owner scrubs them. A pop-then-wake design
    /// permanently lost the wake when the wake side hit the sleeper's
    /// commit window (task transiently not Blocked) — the popped entry
    /// was gone and the task stayed Blocked(Sleep) forever. Peeking
    /// makes wakes at-least-once: a transient failure retries next tick.
    fn collect_due(&self, now_tick: u64, out: &mut [(u32, u64)]) -> usize {
        if self.active_count == 0 {
            return 0;
        }
        let scan = self.scan_bound();
        let mut n = 0;
        for entry in self.entries[..scan].iter() {
            if n >= out.len() {
                break;
            }
            if entry.active && tick_reached(now_tick, entry.wake_tick) {
                out[n] = (entry.task_id, entry.generation);
                n += 1;
            }
        }
        n
    }

    /// Remove the entry for `task_id` only if it is still the arm
    /// generation the caller collected. A mismatch means the owner
    /// re-armed in the meantime — that newer park's entry must survive.
    fn remove_generation(&mut self, task_id: u32, generation: u64) {
        let scan = self.scan_bound();
        for entry in self.entries[..scan].iter_mut() {
            if entry.active && entry.task_id == task_id && entry.generation == generation {
                *entry = SleepEntry::empty();
                self.active_count = self.active_count.saturating_sub(1);
                SLEEP_ACTIVE_COUNT.store(self.active_count, Ordering::Release);
                break;
            }
        }
    }

    /// Record that a due entry's owner was observed not sleep-parked.
    /// Returns `true` once the miss budget is exhausted — a mid-park
    /// transient lasts microseconds, so ~1 s of consecutive due-tick
    /// misses can only be a stale entry (owner event-woken, never
    /// re-armed) and the caller should scrub it.
    fn note_miss(&mut self, task_id: u32, generation: u64) -> bool {
        let scan = self.scan_bound();
        for entry in self.entries[..scan].iter_mut() {
            if entry.active && entry.task_id == task_id && entry.generation == generation {
                entry.misses = entry.misses.saturating_add(1);
                return entry.misses >= 100;
            }
        }
        false
    }

    /// Earliest still-unfired wake deadline (tick units), or `None`
    /// if no tasks are sleeping. Used by the tickless-idle path
    /// (`sched/src/runtime.rs`) to program a one-shot LAPIC timer
    /// for the next deadline before HLT, so a 1 ms kernel-task
    /// sleep does not wait for the next 10 ms periodic tick to
    /// be serviced.
    ///
    /// O(active_high_water). Caller-side fast path: callers should
    /// observe [`SLEEP_ACTIVE_COUNT`] without the lock first; only
    /// take the lock if non-zero.
    fn earliest_deadline(&self, now_tick: u64) -> Option<u64> {
        if self.active_count == 0 {
            return None;
        }
        let scan = self.scan_bound();
        let mut best: Option<u64> = None;
        for entry in self.entries[..scan].iter() {
            if !entry.active {
                continue;
            }
            let candidate = entry.wake_tick;
            best = match best {
                None => Some(candidate),
                Some(b) => {
                    // Closest-to-now: smaller distance forward
                    // wins; deadlines already in the past compare
                    // as wrapping-near-zero by `wrapping_sub`,
                    // which is what we want.
                    let d_b = b.wrapping_sub(now_tick);
                    let d_c = candidate.wrapping_sub(now_tick);
                    if d_c < d_b { Some(candidate) } else { Some(b) }
                }
            };
        }
        best
    }
}

static SLEEP_QUEUE: SpinLock<SleepQueue> = SpinLock::new(SleepQueue::new(), LOCK_LEVEL_REGISTRY);

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

// AUDIT 2A: sleep wait/wake protocol — race-free without harmonic-cascade.
//
// Sleep differs from `task_wait_for` and other multi-observer wait queues:
// there is exactly ONE wake event (the deadline) and exactly ONE observer
// (the sleeper). The status atomic is the single ground truth.
//
// Correctness comes from the CAS chain on `Task::status`:
//   * `block_current_task_with_timeout` arms the deadline, then
//     CAS(Running -> Blocked) under IRQ-off. A racing `unblock_task` on
//     the same task either (a) wins the Blocked->Ready CAS later (we
//     already committed to Blocked), or (b) finds the task still
//     Running and is a no-op — fine, our wait-queue contract puts
//     the waiter on a queue *before* the CAS and the wake side
//     finds it there.
//   * `sleep_current_task_ms` does CAS(Running -> Blocked) under
//     IRQ-off, paired with `wake_sleeping_task`'s
//     CAS-via-`task_set_state_with_reason` from Blocked -> Ready.
//
// Both directions serialise through atomic state CAS. A blocking path
// CASes Running -> Blocked directly under its wait queue's lock, so the
// wake path's Blocked -> Ready CAS sees a committed transition with no
// intermediate state to race against.
/// Outcome of a timer-path wake attempt, deciding what happens to the
/// collected entry: only a CONCLUSIVE outcome may remove it.
enum WakeVerdict {
    /// The wake resolved (published Ready, or the task was observably
    /// awake after being sleep-parked) — the collected generation may go.
    Delivered,
    /// The owner is gone; the entry may go.
    TaskGone,
    /// The owner was not sleep-parked at this instant (mid-park commit
    /// window, or a stale entry). The entry must stay armed for retry.
    NotSleepParked,
}

fn wake_sleeping_task(task_id: u32) -> WakeVerdict {
    if task_id == INVALID_TASK_ID {
        return WakeVerdict::TaskGone;
    }

    let Some(task_ref) = task_find_by_id(task_id) else {
        return WakeVerdict::TaskGone;
    };
    if task_ref.status() == TaskStatus::Invalid || task_ref.is_exited() {
        return WakeVerdict::TaskGone;
    }

    let is_sleep_blocked =
        task_ref.is_blocked() && task_ref.load_block_reason() == BlockReason::Sleep;
    if !is_sleep_blocked {
        // Transient: the task is mid-park (commit window) or already woken
        // with its residual entry not yet re-armed. The entry must stay so
        // the next tick retries — a pop-then-wake design turned exactly
        // this window into a permanent lost wake.
        return WakeVerdict::NotSleepParked;
    }

    // Delegate to the scheduler's single wake publisher. It handles the
    // Linux-style `on_cpu` switch window, reserves scheduler placement before
    // publishing Ready, and queues/inboxes exactly once. Its totality
    // contract guarantees it returns only once the task is published or
    // observably no longer Blocked — so reaching here is conclusive.
    let rc = wake_blocked_task(task_ref.arc(), task_id);
    if rc != 0 {
        slopos_ostd::klog_info!(
            "SCHED: sleep wake failed to publish READY task {} (rc={})",
            task_id,
            rc
        );
        return WakeVerdict::TaskGone;
    }
    WakeVerdict::Delivered
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
/// Lock-free snapshot of the soonest pending wake deadline (or
/// `None` when no tasks are sleeping). Returns the deadline in
/// LAPIC tick units (same domain as
/// `slopos_kernel_services::platform::timer_ticks()`).
///
/// O(1) when the fast path observes [`SLEEP_ACTIVE_COUNT`] == 0;
/// otherwise O(active_high_water) under the queue lock. The
/// idle-loop tickless-arm path consults this every time it would
/// otherwise HLT.
pub fn sleep_queue_next_deadline_ticks(now_tick: u64) -> Option<u64> {
    if SLEEP_ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    SLEEP_QUEUE.lock().earliest_deadline(now_tick)
}

pub fn wake_due_sleepers(now_tick: u64) {
    // Before the fast-path return: a count desync (atomic 0, entries live)
    // would silence this function forever — sweep first so that state is
    // still observable.
    strand_sweep(now_tick);
    if SLEEP_ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    // Peek-then-wake: entries stay armed until a wake conclusively lands
    // (see `collect_due`). Bounded batch per tick; the remainder is
    // retried on the next tick. Concurrent ticks on peer CPUs may collect
    // the same entry — `wake_blocked_task`'s placement CAS is
    // single-winner, so duplicates are no-ops.
    //
    // Removal discipline: ONLY this timer path (generation-checked, after a
    // conclusive wake) and the owner task itself remove entries. Event
    // wakes never touch the queue — a waker-side cancel after publishing
    // Ready raced the owner's next re-arm and deleted the wrong
    // generation's entry (the `net-timer` strand).
    let mut due = [(INVALID_TASK_ID, 0u64); 16];
    let n = {
        let queue = SLEEP_QUEUE.lock();
        queue.collect_due(now_tick, &mut due)
    };
    for &(task_id, generation) in &due[..n] {
        match wake_sleeping_task(task_id) {
            WakeVerdict::Delivered | WakeVerdict::TaskGone => {
                SLEEP_QUEUE.lock().remove_generation(task_id, generation);
            }
            WakeVerdict::NotSleepParked => {
                // Keep the entry for retry; scrub only once the miss budget
                // proves it stale (owner event-woken and never re-armed).
                if SLEEP_QUEUE.lock().note_miss(task_id, generation) {
                    SLEEP_QUEUE.lock().remove_generation(task_id, generation);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Strand sweep — lost-wake diagnostics (armed by `tp.debug` boots)
// ---------------------------------------------------------------------------

static STRAND_SWEEP_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static STRAND_LAST_SWEEP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static STRAND_LOG_BUDGET: AtomicU32 = AtomicU32::new(120);

/// Arm the ~once-per-second stranded-task sweep.
pub fn arm_strand_sweep() {
    STRAND_SWEEP_ARMED.store(true, Ordering::Release);
}

fn strand_log_ok() -> bool {
    STRAND_LOG_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if v > 0 { Some(v - 1) } else { None }
        })
        .is_ok()
}

/// Scan for tasks stranded by a lost sleep wake: Blocked-on-Sleep with no
/// armed queue entry (the wake was popped/wiped but never delivered), an
/// overdue entry (pop never fires), a Ready task with no scheduler placement
/// (publish lost), and sleep-queue count desync (fast path permanently
/// silenced). Tick context; ~1 Hz via the tick-spacing gate.
fn strand_sweep(now_tick: u64) {
    if !STRAND_SWEEP_ARMED.load(Ordering::Acquire) {
        return;
    }
    let last = STRAND_LAST_SWEEP.load(Ordering::Relaxed);
    if now_tick.wrapping_sub(last) < 400 {
        return;
    }
    if STRAND_LAST_SWEEP
        .compare_exchange(last, now_tick, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let (live, count, atomic) = {
        let queue = SLEEP_QUEUE.lock();
        let scan = queue.scan_bound();
        let live = queue.entries[..scan].iter().filter(|e| e.active).count() as u32;
        (
            live,
            queue.active_count,
            SLEEP_ACTIVE_COUNT.load(Ordering::Acquire),
        )
    };
    if (live != count || atomic != count) && strand_log_ok() {
        slopos_ostd::klog_info!(
            "STRAND: sleep-queue desync live={} count={} atomic={}",
            live,
            count,
            atomic
        );
    }

    super::task::task_for_each_active(|task| strand_sweep_task(task, now_tick));
}

fn strand_sweep_task(task: &super::task::Task, now_tick: u64) {
    if task.status() == TaskStatus::Invalid || task.is_exited() {
        return;
    }
    let task_id = task.task_id;
    let status = task.status();
    let mut class = 0u8;
    let mut detail = 0u64;
    if status == TaskStatus::Blocked && task.load_block_reason() == BlockReason::Sleep {
        let entry = {
            let queue = SLEEP_QUEUE.lock();
            let scan = queue.scan_bound();
            queue.entries[..scan]
                .iter()
                .find(|entry| entry.active && entry.task_id == task_id)
                .map(|entry| entry.wake_tick)
        };
        match entry {
            None if task.sched_placement() == slopos_ostd::task::SchedPlacement::None => {
                class = 1;
            }
            None => {}
            Some(wake_tick)
                if now_tick.wrapping_sub(wake_tick) < (1 << 63)
                    && now_tick.wrapping_sub(wake_tick) > 400 =>
            {
                class = 2;
                detail = wake_tick;
            }
            _ => {}
        }
    } else if status == TaskStatus::Ready
        && task.sched_placement() == slopos_ostd::task::SchedPlacement::None
    {
        class = 3;
    }

    let idx = task_id as usize % STRAND_SUSPECT.len();
    let previous_id = STRAND_TASK_ID[idx].swap(task_id, Ordering::Relaxed);
    if previous_id != task_id {
        STRAND_SUSPECT[idx].store(0, Ordering::Relaxed);
        STRAND_EPOCH[idx].store(0, Ordering::Relaxed);
    }
    let previous_class = STRAND_SUSPECT[idx].swap(class, Ordering::Relaxed);
    let epoch = task.state_epoch();
    let previous_epoch = STRAND_EPOCH[idx].swap(epoch, Ordering::Relaxed);
    if class == 0 || previous_class != class || previous_epoch != epoch || !strand_log_ok() {
        return;
    }
    match class {
        1 => slopos_ostd::klog_info!(
            "STRAND: task {} '{}' Blocked(Sleep) NO ENTRY now={} placement={:?}",
            task_id,
            task_name_str(task),
            now_tick,
            task.sched_placement()
        ),
        2 => slopos_ostd::klog_info!(
            "STRAND: task {} '{}' entry OVERDUE wake@{} now={}",
            task_id,
            task_name_str(task),
            detail,
            now_tick
        ),
        _ => slopos_ostd::klog_info!(
            "STRAND: task {} '{}' Ready with placement=None (publish lost)",
            task_id,
            task_name_str(task)
        ),
    }
}

/// Per-task suspect class from the previous sweep (persistence filter),
/// indexed by task id.
static STRAND_SUSPECT: [core::sync::atomic::AtomicU8; MAX_TASKS] = {
    const ZERO: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    [ZERO; MAX_TASKS]
};

static STRAND_TASK_ID: [AtomicU32; MAX_TASKS] = {
    const EMPTY: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);
    [EMPTY; MAX_TASKS]
};

/// Per-task state-word epoch from the previous sweep (see the epoch gate in
/// [`strand_sweep`]).
static STRAND_EPOCH: [AtomicU32; MAX_TASKS] = {
    const ZERO: AtomicU32 = AtomicU32::new(0);
    [ZERO; MAX_TASKS]
};

fn task_name_str(task: &super::task::Task) -> &str {
    core::str::from_utf8(task.name_bytes()).unwrap_or("?")
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

/// Arm a millisecond-resolution wake deadline for a task that is
/// already `Blocked` (the wait-queue protocol's lock-held CAS has
/// already committed the state). The timer-tick callback
/// (`wake_due_sleepers`) will CAS `Blocked → Ready` when the deadline
/// fires. Idempotent: a second call before the first deadline
/// expires updates the wake tick in place.
///
/// Stamps `BlockReason::Sleep` on the task so `wake_sleeping_task`
/// recognises this as a sleep-queue wake when the deadline fires
/// (it gates on reason==Sleep to avoid spurious wakes on a task
/// that has since re-blocked for a different reason).
pub fn arm_blocked_timeout(task_id: u32, timeout_ms: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    if let Some(task) = task_find_by_id(task_id) {
        // The guard derefs to `&Task`; the store is a relaxed atomic on the
        // fused state word.
        task.store_block_reason(BlockReason::Sleep);
    }
    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(timeout_ms));
    SLEEP_QUEUE.lock().upsert(task_id, wake_tick);
}

pub fn sleep_current_task_ms(ms: u32) -> c_int {
    if ms == 0 {
        return 0;
    }

    if !is_scheduling_active() {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let Some(current) = crate::task_struct::Current::get() else {
        return -1;
    };
    if slopos_ostd::task::TaskAddr::current().is_some_and(super::per_cpu::is_idle_task) {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let task_id = current.id();

    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(ms));

    // See `block_current_task_with_timeout` for why CAS precedes upsert.
    let rc = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| -> c_int {
        if task_set_state_from_with_reason(
            task_id,
            TaskStatus::Running,
            TaskStatus::Blocked,
            BlockReason::Sleep,
        ) != 0
        {
            // A wake can make the still-executing current task Ready before
            // it tries to block again. Consume that wake here: restore the
            // in-CPU state to Running and scrub any stale runqueue entry, so
            // we do not continue executing indefinitely as Ready/unqueued.
            if current.task().status() == TaskStatus::Ready {
                consume_ready_wake_for_current(&current);
                return 1;
            }
            return -1;
        }
        if !SLEEP_QUEUE.lock().upsert(task_id, wake_tick) {
            let _ = super::task::task_try_transition_from(
                task_id,
                TaskStatus::Blocked,
                TaskStatus::Running,
            );
            return -1;
        }
        if !commit_blocked_deschedule(&current) {
            return 1; // raced: a wake was consumed; scrub the sleep entry below
        }
        schedule();
        0
    });
    if rc == 1 {
        cancel_sleep(task_id);
        return 0;
    }
    rc
}

/// Block the current task with a timeout.
///
/// Combines scheduler-backed blocking (Running→Blocked CAS + yield semantics)
/// with sleep-queue timeout (`sleep_current_task_ms` semantics).
///
/// The caller is expected to have registered a wakeup mechanism (e.g.
/// storing the task handle so an IRQ can call `unblock_task`). This
/// function provides the timeout safety net: if the external signal
/// never arrives, the sleep timer wakes us after `timeout_ms`.
///
/// # Safety contract
/// Must NOT be called while holding an `SpinLock` — blocking with a
/// mutex held risks deadlock if another task contends the same lock.
pub fn block_current_task_with_timeout(timeout_ms: u32) {
    if !is_scheduling_active() {
        // No scheduler — fall through; the caller's spin fallback handles it.
        return;
    }

    let Some(current) = crate::task_struct::Current::get() else {
        return;
    };
    if slopos_ostd::task::TaskAddr::current().is_some_and(super::per_cpu::is_idle_task) {
        return;
    }

    let task_id = current.id();

    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(timeout_ms));

    // CAS Running→Blocked must happen before SLEEP_QUEUE.upsert.
    // If upsert came first, a peer CPU's `wake_due_sleepers` could
    // pop our entry while we are still Running, drop the wake on the
    // floor (`wake_sleeping_task` gates on `is_blocked`), and leave
    // us about to CAS into Blocked with no wakeup armed. Mirrors
    // Linux's `set_current_state` → `hrtimer_start` ordering.
    let cancelled = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| -> bool {
        if task_set_state_from_with_reason(
            task_id,
            TaskStatus::Running,
            TaskStatus::Blocked,
            BlockReason::Sleep,
        ) != 0
        {
            // Same consumed-wake case as `sleep_current_task_ms`: the
            // current task is still executing, but a prior wake has already
            // published Ready. Convert it back to Running and retry from the
            // caller instead of leaving a Ready current task for the rescue
            // sweep to rediscover.
            if current.task().status() == TaskStatus::Ready {
                consume_ready_wake_for_current(&current);
            }
            return true;
        }
        if !SLEEP_QUEUE.lock().upsert(task_id, wake_tick) {
            let _ = super::task::task_try_transition_from(
                task_id,
                TaskStatus::Blocked,
                TaskStatus::Running,
            );
            return true;
        }
        if !commit_blocked_deschedule(&current) {
            return false; // raced: a wake was consumed; cancel_sleep below scrubs
        }
        schedule();
        false
    });
    if cancelled {
        return;
    }
    cancel_sleep(task_id);
}

#[cfg(feature = "test-hooks")]
pub(crate) fn test_insert_sleep_entry(task_id: u32, wake_tick: u64) -> bool {
    SLEEP_QUEUE.lock().upsert(task_id, wake_tick)
}

/// Test hook: whether an armed sleep entry exists for `task_id`.
#[cfg(feature = "test-hooks")]
pub(crate) fn test_sleep_entry_armed(task_id: u32) -> bool {
    let queue = SLEEP_QUEUE.lock();
    let scan = queue.scan_bound();
    queue.entries[..scan]
        .iter()
        .any(|e| e.active && e.task_id == task_id)
}
