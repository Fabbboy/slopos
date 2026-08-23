use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::task::BlockReason;
use slopos_ostd::KVec;
use slopos_ostd::sync::kernel_io_task::KernelIoTaskIds;
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
    /// Arm generation, bumped on every `upsert`. Timer-path removal is
    /// generation-checked, so a wake against one park cannot delete the entry
    /// of a later park re-armed on another CPU.
    generation: u64,
    /// Consecutive due-ticks on which the owner was observed not sleep-parked.
    /// A mid-park transient lasts microseconds, so a large count means a stale
    /// entry (owner event-woken, never re-armed).
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

/// Sleep queue backed by a heap `KVec`, pre-reserved to `MAX_TASKS` and never
/// reallocated afterwards; entries are mutated in place.
///
/// `active_count` lets the timer tick skip the scan entirely when nothing is
/// sleeping, and `active_high_water` bounds it to the largest slot index ever
/// occupied, so a quiesced queue still scans O(peak) rather than O(capacity).
struct SleepQueue {
    entries: KVec<SleepEntry>,
    active_count: u32,
    active_high_water: u32,
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

    /// Keeps the deadlines registered kernel-I/O threads own: those are the
    /// *only* wake source some have — net-timer is woken by nothing but its own
    /// 50 ms park — and a wiped one leaves it `Blocked` forever.
    fn reset_preserving(&mut self, kernel_io: &KernelIoTaskIds) -> Result<(), ()> {
        if self.entries.is_empty() {
            if self.entries.try_reserve_exact(MAX_TASKS).is_err() {
                return Err(());
            }
            for _ in 0..MAX_TASKS {
                if self.entries.push(SleepEntry::empty()).is_err() {
                    return Err(());
                }
            }
            self.active_count = 0;
            self.active_high_water = 0;
            SLEEP_ACTIVE_COUNT.store(0, Ordering::Release);
            return Ok(());
        }

        let mut kept = 0u32;
        let mut high_water = 0u32;
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            if entry.active && kernel_io.contains(entry.task_id) {
                kept += 1;
                high_water = (idx as u32) + 1;
                continue;
            }
            *entry = SleepEntry::empty();
        }
        self.active_count = kept;
        self.active_high_water = high_water;
        SLEEP_ACTIVE_COUNT.store(kept, Ordering::Release);
        Ok(())
    }

    fn init_or_reset(&mut self) -> Result<(), ()> {
        self.reset_preserving(&KernelIoTaskIds::empty())
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

    /// Collect the ids of due entries into `out` WITHOUT removing them; an
    /// entry only goes once a wake conclusively publishes `Ready` or the owner
    /// scrubs it. Peeking is what makes wakes at-least-once — a wake that lands
    /// in the sleeper's commit window retries on the next tick instead of being
    /// lost with the popped entry.
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

    /// Record that a due entry's owner was observed not sleep-parked. Returns
    /// `true` once ~1 s of consecutive misses proves the entry stale (owner
    /// event-woken, never re-armed) and the caller should scrub it.
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

    /// Earliest still-unfired wake deadline (tick units), or `None` if no task
    /// is sleeping. The tickless-idle path programs a one-shot LAPIC timer from
    /// this so a 1 ms sleep is not rounded up to the next periodic tick.
    ///
    /// O(active_high_water); callers should observe [`SLEEP_ACTIVE_COUNT`]
    /// lock-free first and only take the lock if non-zero.
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
                    // Smallest forward distance wins; `wrapping_sub` makes an
                    // already-past deadline compare near zero, which is right.
                    let d_b = b.wrapping_sub(now_tick);
                    let d_c = candidate.wrapping_sub(now_tick);
                    if d_c < d_b { Some(candidate) } else { Some(b) }
                }
            };
        }
        best
    }
}

static SLEEP_QUEUE: SpinLock<SleepQueue> = SpinLock::new(
    SleepQueue::new(),
    lock_class!("SLEEP_QUEUE", LOCK_LEVEL_REGISTRY),
);

/// External mirror of `SleepQueue::active_count`, written under the
/// `SLEEP_QUEUE` lock but readable without it, so the timer tick can skip the
/// lock entirely while nothing is sleeping. A sleeper added between the
/// lock-free load and the lock acquire is simply picked up on the next tick.
static SLEEP_ACTIVE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Initialise (or reset) the sleep queue; safe on both first boot and
/// test-fixture re-init.
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

/// Outcome of a timer-path wake attempt: only a conclusive outcome may remove
/// the collected entry.
enum WakeVerdict {
    /// The wake resolved; the collected generation may go.
    Delivered,
    /// The owner is gone; the entry may go.
    TaskGone,
    /// The owner was not sleep-parked at this instant (mid-park commit window,
    /// or a stale entry). The entry must stay armed for retry.
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
        return WakeVerdict::NotSleepParked;
    }

    let rc = wake_blocked_task(&task_ref, task_id);
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

/// Soonest pending wake deadline, in the same tick domain as
/// `slopos_kernel_services::platform::timer_ticks()`, or `None` when no task is
/// sleeping. O(1) while [`SLEEP_ACTIVE_COUNT`] is zero, otherwise
/// O(active_high_water) under the queue lock.
pub fn sleep_queue_next_deadline_ticks(now_tick: u64) -> Option<u64> {
    if SLEEP_ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    SLEEP_QUEUE.lock().earliest_deadline(now_tick)
}

/// Wake every sleeper whose deadline has passed. Runs on every CPU's timer
/// tick, so it returns without touching `SLEEP_QUEUE` while nothing sleeps.
pub fn wake_due_sleepers(now_tick: u64) {
    // Ahead of the fast-path return: a count desync (atomic 0, entries live)
    // would otherwise silence this function, and the sweep, forever.
    strand_sweep(now_tick);
    if SLEEP_ACTIVE_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    // Bounded batch per tick; the remainder is retried next tick. Peer CPUs may
    // collect the same entry — `wake_blocked_task`'s placement CAS is
    // single-winner, so duplicates are no-ops.
    //
    // Removal discipline: only this generation-checked timer path and the owner
    // task itself remove entries. A waker-side cancel would race the owner's
    // next re-arm and delete the wrong generation's entry.
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
                if SLEEP_QUEUE.lock().note_miss(task_id, generation) {
                    SLEEP_QUEUE.lock().remove_generation(task_id, generation);
                }
            }
        }
    }
}

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

/// Scan for tasks stranded by a lost sleep wake: Blocked-on-Sleep with no armed
/// entry, an overdue entry, a Ready task with no scheduler placement, and
/// sleep-queue count desync. Tick context; ~1 Hz via the tick-spacing gate.
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

/// Previous sweep's suspect class per task id — the persistence filter that
/// stops a one-off transient from logging.
static STRAND_SUSPECT: [core::sync::atomic::AtomicU8; MAX_TASKS] = {
    const ZERO: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    [ZERO; MAX_TASKS]
};

static STRAND_TASK_ID: [AtomicU32; MAX_TASKS] = {
    const EMPTY: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);
    [EMPTY; MAX_TASKS]
};

/// Previous sweep's state-word epoch per task id; a change means the task moved
/// and the suspicion is stale.
static STRAND_EPOCH: [AtomicU32; MAX_TASKS] = {
    const ZERO: AtomicU32 = AtomicU32::new(0);
    [ZERO; MAX_TASKS]
};

fn task_name_str(task: &super::task::Task) -> &str {
    core::str::from_utf8(task.name_bytes()).unwrap_or("?")
}

/// Snapshot before the queue lock: the stop registry is the same lock level.
pub fn reset_sleep_queue() {
    let kernel_io = slopos_ostd::sync::kernel_io_task::kernel_io_task_ids();
    SLEEP_QUEUE.lock().reset_preserving(&kernel_io).ok();
}

pub fn reset_sleep_queue_preserving_kernel_io() {
    reset_sleep_queue();
}

pub fn cancel_sleep(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    SLEEP_QUEUE.lock().remove(task_id);
}

/// Arm a millisecond-resolution wake deadline for a task already committed to
/// `Blocked`. Idempotent: re-arming before the deadline updates the wake tick
/// in place. Stamps `BlockReason::Sleep` so the timer path does not wake a task
/// that has since re-blocked for a different reason.
///
/// Returns whether a deadline is actually armed. `false` means the queue has no
/// backing store yet or every slot is taken; the caller has already committed
/// `Running → Blocked` by then, so on `false` it **must not deschedule** —
/// nothing would ever wake the task again.
#[must_use = "a caller that deschedules on a failed arm parks its task forever"]
pub fn arm_blocked_timeout(task_id: u32, timeout_ms: u32) -> bool {
    if task_id == INVALID_TASK_ID {
        return false;
    }
    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(timeout_ms));
    if !SLEEP_QUEUE.lock().upsert(task_id, wake_tick) {
        return false;
    }
    // Stamped only once the entry is in, keeping `Blocked(Sleep) ⇔ a deadline
    // is armed` true in both directions. A tick landing in the window before
    // the stamp reads the owner as not sleep-parked and retries next tick.
    if let Some(task) = task_find_by_id(task_id) {
        task.store_block_reason(BlockReason::Sleep);
    }
    true
}

/// Give the sleep queue its backing store if it has none, leaving any entries
/// it already holds alone. Called from the task-allocation path, the one point
/// that necessarily precedes any park.
///
/// Entries are built outside the lock and the displaced buffer is dropped after
/// the guard: `SLEEP_QUEUE` is cli-disabling and the allocator is where every
/// subsystem meets.
pub(crate) fn ensure_sleep_queue_allocated() -> bool {
    if SLEEP_QUEUE.lock().entries.len() == MAX_TASKS {
        return true;
    }
    let mut entries = match KVec::with_capacity(MAX_TASKS) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    for _ in 0..MAX_TASKS {
        if entries.push(SleepEntry::empty()).is_err() {
            return false;
        }
    }
    let leftover = {
        let mut queue = SLEEP_QUEUE.lock();
        if queue.entries.len() == MAX_TASKS {
            // Lost the race; ours is the buffer that gets freed.
            entries
        } else {
            queue.active_count = 0;
            queue.active_high_water = 0;
            SLEEP_ACTIVE_COUNT.store(0, Ordering::Release);
            core::mem::replace(&mut queue.entries, entries)
        }
    };
    drop(leftover);
    true
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
/// The caller is expected to have registered its own wakeup mechanism; this
/// provides only the safety net that wakes us after `timeout_ms` if the
/// external signal never arrives.
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

    // CAS Running→Blocked must precede the upsert. The other order lets a peer
    // CPU's tick collect our entry while we are still Running, drop the wake
    // (the timer path gates on `is_blocked`), and leave us about to CAS into
    // Blocked with nothing armed to wake us.
    let cancelled = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| -> bool {
        if task_set_state_from_with_reason(
            task_id,
            TaskStatus::Running,
            TaskStatus::Blocked,
            BlockReason::Sleep,
        ) != 0
        {
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

#[cfg(feature = "test-hooks")]
pub(crate) fn test_sleep_entry_armed(task_id: u32) -> bool {
    let queue = SLEEP_QUEUE.lock();
    let scan = queue.scan_bound();
    queue.entries[..scan]
        .iter()
        .any(|e| e.active && e.task_id == task_id)
}
