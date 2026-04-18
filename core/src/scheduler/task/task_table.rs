use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_alloc::{KBox, KVec};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY};
use slopos_utils::string::bytes_as_str;
use slopos_utils::{klog_debug, klog_info};

use super::super::scheduler;
use super::{INVALID_PROCESS_ID, INVALID_TASK_ID, Task, TaskExitRecord, TaskIterateCb, TaskStatus};

/// Concurrent task capacity, matching the kernel-stack VA region cap in
/// `mm/src/memory_layout_defs.rs` — every live task owns a KSTACK VA
/// slot, so the task pool cannot usefully exceed the number of KSTACK
/// slots. Growing beyond this requires expanding the KSTACK VA window.
pub const TASK_POOL_CAPACITY: usize = 8192;

// =============================================================================
// Zombie List for Deferred Task Reclamation
// =============================================================================

/// List of terminated tasks waiting for the zombie reaper to reset them
/// once `refcnt == 0`. Protected by `IrqMutex` for interrupt safety.
/// The `KVec` is pre-reserved to `TASK_POOL_CAPACITY` at init time so
/// pushes never allocate under the lock.
static ZOMBIE_LIST: IrqMutex<ZombieList> = IrqMutex::new(ZombieList::new(), LOCK_LEVEL_REGISTRY);

/// High-water mark of pool-slot indices that have ever been populated
/// with a `KBox<Task>`. Monotonically increases; only written in
/// tier-3 of [`reserve_task_slot`] when a fresh slot is allocated past
/// the current HWM. Iteration bounds (`reserve_task_slot` tier scans,
/// `task_find_by_id`, `task_find_by_cr3`, `task_iterate_active`,
/// `task_slot_census`) load this with `Acquire` and only walk
/// `0..hwm` — O(peak-concurrent-tasks) rather than
/// O(`TASK_POOL_CAPACITY`). Because slots never transition `Some →
/// None` during normal operation, the HWM is a safe upper bound: any
/// live pointer lives at an index strictly below `hwm`, and
/// lower-indexed `None` slots are simply harmless skips.
static POOL_HIGH_WATER: AtomicU32 = AtomicU32::new(0);

#[inline]
pub(super) fn pool_high_water() -> usize {
    POOL_HIGH_WATER.load(Ordering::Acquire) as usize
}

#[inline]
fn bump_pool_high_water(new_idx: usize) {
    let new_hwm = (new_idx as u32).saturating_add(1);
    let mut cur = POOL_HIGH_WATER.load(Ordering::Relaxed);
    while cur < new_hwm {
        match POOL_HIGH_WATER.compare_exchange_weak(
            cur,
            new_hwm,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => cur = observed,
        }
    }
}

struct ZombieList {
    tasks: KVec<*mut Task>,
}

// SAFETY: ZombieList contains raw pointers into stable pool slots.
// All access is serialised through the IrqMutex.
unsafe impl Send for ZombieList {}
unsafe impl Sync for ZombieList {}

impl ZombieList {
    const fn new() -> Self {
        Self { tasks: KVec::new() }
    }

    /// Best-effort push. If the KVec's pre-reserved capacity has been
    /// exhausted (pathological: every slot is a refcount-held zombie)
    /// and `try_reserve` fails, we log and drop the zombie on the
    /// floor; the slot remains `Terminated` and will be reclaimed by a
    /// future `reserve_task_slot` tier-2 scan once `refcnt` reaches 0.
    fn push(&mut self, task: *mut Task) {
        if self.tasks.push(task).is_err() {
            klog_info!("zombie_list: push failed, leaving task in Terminated state");
        }
    }
}

/// Add a terminated task to the zombie list for deferred cleanup.
/// The task slot will be reset when its reference count reaches zero.
pub(super) fn defer_task_cleanup(task: *mut Task) {
    if task.is_null() {
        return;
    }
    ZOMBIE_LIST.lock().push(task);
}

/// Free a task's kernel-mode stack without invalidating the task struct.
///
/// Dropping `task.kernel_stack = None` runs the `KernelStack::drop`
/// handler: releases the VA slot and physical frames via the per-CPU
/// kstack cache. All automatic; no manual `kfree`.
///
/// The slot remains in its current status (typically Terminated) so
/// that `task_find_by_id` can still locate it for idempotent terminate
/// calls.
///
/// User-space stacks live in the owning process's VM and are reclaimed
/// by `destroy_process_vm`, not here.
pub(super) fn free_task_stacks(task: *mut Task) {
    if task.is_null() {
        return;
    }
    unsafe {
        // Dropping the handle releases the stack's VA slot + physical frames.
        (*task).kernel_stack = None;
        (*task).kernel_stack_base = 0;
        (*task).kernel_stack_top = 0;
        (*task).kernel_stack_size = 0;

        // For kernel-mode tasks, `stack_base` aliased the kernel stack.
        // Now that the stack is gone, clear the alias so nothing reads it.
        if (*task).process_id == INVALID_PROCESS_ID {
            (*task).stack_base = 0;
        }
    }
}

pub(super) fn free_task_memory_and_invalidate(task: *mut Task) {
    if task.is_null() {
        return;
    }
    free_task_stacks(task);
    unsafe {
        Task::reset_in_place(task);
    }
}

/// Reap zombie tasks that are ready to be reset.
/// Should be called periodically (e.g., from scheduler idle path).
///
/// Invariant: this does NOT drop the owning `KBox<Task>` — the pool
/// keeps KBoxes alive until kernel shutdown so lock-free readers never
/// observe a freed backing allocation. It only resets the Task struct
/// in place (via `free_task_memory_and_invalidate`) so the slot becomes
/// reusable for future allocations.
///
/// **Hot-path constraint**: this runs on every iteration of every
/// CPU's scheduler idle loop, so the work MUST be bounded by the
/// zombie-list length (typically a handful of entries) — never the
/// pool size. Lazily-Terminated slots (tasks that skipped the zombie
/// list because `refcnt == 0` at cleanup) are reclaimed on demand by
/// tier-2 of [`reserve_task_slot`]; the pool can sit in a Terminated
/// steady state between allocations without leaking anything (kstacks
/// are released at termination via `free_task_stacks`, not at reset).
pub fn reap_zombies() {
    let mut list = ZOMBIE_LIST.lock();
    let original_count = list.tasks.len();
    if original_count == 0 {
        return;
    }
    let mut write_idx = 0usize;
    for read_idx in 0..original_count {
        let task = list.tasks[read_idx];
        if task.is_null() {
            continue;
        }
        let ref_count = unsafe { (*task).ref_count() };
        if ref_count == 0 {
            unsafe {
                klog_debug!("reap_zombies: resetting zombie task {}", (*task).task_id);
            }
            free_task_memory_and_invalidate(task);
        } else {
            if write_idx != read_idx {
                list.tasks[write_idx] = task;
            }
            write_idx += 1;
        }
    }
    list.tasks.truncate(write_idx);
}

// =============================================================================
// TaskManagerInner — dynamic heap-backed task pool
// =============================================================================

pub(super) struct TaskManagerInner {
    /// Fixed-capacity pool of task slots. The backing `KVec` is
    /// pre-reserved to `TASK_POOL_CAPACITY` at `init_task_manager`
    /// time and never reallocates afterwards, so pointers into slot
    /// bodies (held by the scheduler, ready queues, per-CPU
    /// current-task caches, assembly switch routines) remain valid for
    /// each Task's lifetime.
    ///
    /// Each slot is one of:
    /// - `None`: pristine, never allocated a Task (tier-3 target).
    /// - `Some(kbox)` with `status == Invalid`: reusable (tier-1 target).
    /// - `Some(kbox)` with `status == Terminated` and `refcnt == 0`:
    ///   reap candidate (tier-2 target).
    /// - `Some(kbox)` with any live status: in-use.
    ///
    /// A slot never transitions `Some → None` during normal operation.
    /// The `KBox` lives until kernel shutdown; recycling happens via
    /// `Task::reset_in_place` on the KBox's contents.
    pub(super) tasks: KVec<Option<KBox<Task>>>,
    /// Exit-record cache, parallel-indexed with `tasks`. Length equals
    /// `tasks.len()` after init. Each entry is overwritten when a slot
    /// transitions through Terminated; stale entries carry
    /// `task_id == INVALID_TASK_ID`.
    pub(super) exit_records: KVec<TaskExitRecord>,
    pub(super) num_tasks: u32,
    pub(super) next_task_id: u32,
    pub(super) total_context_switches: u64,
    pub(super) total_yields: u64,
    pub(super) tasks_created: u32,
    pub(super) tasks_terminated: u32,
    pub(super) initialized: bool,
}

// SAFETY: TaskManagerInner contains Tasks (boxed) with raw pointers.
// Cross-CPU access is serialised through the IrqMutex, with the
// documented lock-free read exceptions below.
unsafe impl Send for TaskManagerInner {}

impl TaskManagerInner {
    const fn new() -> Self {
        Self {
            tasks: KVec::new(),
            exit_records: KVec::new(),
            num_tasks: 0,
            next_task_id: 1,
            total_context_switches: 0,
            total_yields: 0,
            tasks_created: 0,
            tasks_terminated: 0,
            initialized: false,
        }
    }

    /// Iterate every occupied pool slot, yielding `&Task`. Skips `None`
    /// entries. Does not filter by status — callers interested in
    /// only-live or only-active tasks must check `task.status()`.
    ///
    /// Scan bound is the global pool high-water mark (slots that have
    /// ever been populated), not the full spine length — crucial for
    /// latency on hot paths under the manager lock.
    pub(super) fn iter_tasks(&self) -> impl Iterator<Item = &Task> + '_ {
        let bound = pool_high_water().min(self.tasks.len());
        self.tasks[..bound]
            .iter()
            .filter_map(|slot| slot.as_deref())
    }

    /// Mutable variant of [`Self::iter_tasks`].
    pub(super) fn iter_tasks_mut(&mut self) -> impl Iterator<Item = &mut Task> + '_ {
        let bound = pool_high_water().min(self.tasks.len());
        self.tasks[..bound]
            .iter_mut()
            .filter_map(|slot| slot.as_deref_mut())
    }
}

static TASK_MANAGER: IrqMutex<TaskManagerInner> =
    IrqMutex::new(TaskManagerInner::new(), LOCK_LEVEL_REGISTRY);

#[inline]
pub(super) fn with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> R {
    let mut guard = TASK_MANAGER.lock();
    f(&mut guard)
}

#[inline]
pub(super) fn try_with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> Option<R> {
    let mut guard = TASK_MANAGER.lock();
    if guard.initialized {
        Some(f(&mut guard))
    } else {
        None
    }
}

/// Ensure the task-manager pool spines are allocated. Safe to call
/// multiple times — only the first invocation performs the big
/// allocations; subsequent calls are cheap no-ops.
///
/// APs bring their per-CPU idle task up during the Drivers boot phase
/// (`smp` init step, priority 45), while `init_task_manager` itself
/// is scheduled in the Services phase (priority 20 — runs *after*
/// Drivers). This helper is invoked from any entry that might reach
/// the pool before `init_task_manager` runs (notably
/// `reserve_task_slot`), so AP idle-task creation succeeds regardless
/// of where it lands in the boot ordering.
fn ensure_pool_allocated() -> bool {
    let already_sized = with_task_manager(|mgr| !mgr.tasks.is_empty());
    if already_sized {
        return true;
    }

    // Allocate spines outside the lock.
    let mut tasks: KVec<Option<KBox<Task>>> = match KVec::with_capacity(TASK_POOL_CAPACITY) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut exit_records: KVec<TaskExitRecord> = match KVec::with_capacity(TASK_POOL_CAPACITY) {
        Ok(v) => v,
        Err(_) => return false,
    };
    for _ in 0..TASK_POOL_CAPACITY {
        if tasks.push(None).is_err() {
            return false;
        }
        if exit_records.push(TaskExitRecord::empty()).is_err() {
            return false;
        }
    }
    // Pre-reserve the zombie list so pushes never allocate under its
    // own IrqMutex.
    {
        let mut zombies = ZOMBIE_LIST.lock();
        if zombies.tasks.capacity() < TASK_POOL_CAPACITY
            && zombies.tasks.try_reserve_exact(TASK_POOL_CAPACITY).is_err()
        {
            return false;
        }
    }
    // Install under the manager lock. Double-check emptiness in case
    // another CPU raced us to init (single-CPU at this boot stage in
    // practice, but stay race-safe).
    let installed = with_task_manager(|mgr| {
        if mgr.tasks.is_empty() {
            mgr.tasks = tasks;
            mgr.exit_records = exit_records;
            true
        } else {
            false
        }
    });
    // Seeding the sleep queue here rather than inside init_task_manager
    // means early IRQ paths that block with timeouts survive even if
    // they're exercised before the service-phase init.
    super::super::sleep::init_sleep_queue();
    if installed {
        TASK_MANAGER.clear_poison();
    }
    // If a race lost, our freshly-allocated spines drop harmlessly.
    true
}

/// Initialise or re-initialise the task manager.
///
/// First-ever call allocates the pool spines (two `KVec`s pre-reserved
/// to `TASK_POOL_CAPACITY`). Subsequent calls (primarily from test
/// fixtures resetting between tests) preserve idle-task slots and
/// reset every other live Task in place.
pub fn init_task_manager() -> c_int {
    if !ensure_pool_allocated() {
        return -1;
    }
    let was_initialized = with_task_manager(|mgr| mgr.initialized);
    if !was_initialized {
        // First-ever call: simply flip the `initialized` flag and
        // clear the manager lock's poison bit. We must NOT reset
        // `num_tasks` / `next_task_id` / counters here — APs that
        // came up during the Drivers phase already allocated their
        // idle tasks via the lazy-init path in `reserve_task_slot`,
        // and those bookkeeping fields already reflect them.
        with_task_manager(|mgr| mgr.initialized = true);
        TASK_MANAGER.clear_poison();
        return 0;
    }

    // Re-init path (tests): preserve idle tasks, reset everything else.
    with_task_manager(|mgr| {
        mgr.total_context_switches = 0;
        mgr.total_yields = 0;
        mgr.tasks_created = 0;
        mgr.tasks_terminated = 0;

        let mut preserved_count: u32 = 0;
        let mut max_task_id: u32 = 0;
        for slot in mgr.tasks.iter_mut() {
            let Some(kbox) = slot.as_deref_mut() else {
                continue;
            };
            let task_ptr = kbox as *mut Task;
            if crate::per_cpu::is_idle_task(task_ptr) {
                preserved_count += 1;
                if kbox.task_id != INVALID_TASK_ID && kbox.task_id > max_task_id {
                    max_task_id = kbox.task_id;
                }
                klog_debug!(
                    "init_task_manager: preserving idle task {} ('{}')",
                    kbox.task_id,
                    bytes_as_str(&kbox.name)
                );
                continue;
            }
            // SAFETY: exclusive `&mut` under the manager lock.
            unsafe { Task::reset_in_place(task_ptr) };
        }
        for rec in mgr.exit_records.iter_mut() {
            *rec = TaskExitRecord::empty();
        }
        mgr.num_tasks = preserved_count;
        mgr.next_task_id = max_task_id.saturating_add(1);
        mgr.initialized = true;
    });
    TASK_MANAGER.clear_poison();
    0
}

/// Find a task by its unique ID.
///
/// **Lock-free fast path**: scans the pool directly without taking the
/// `TASK_MANAGER` lock. Safety rests on three invariants:
///
/// 1. The `tasks` KVec's backing buffer is allocated once in
///    `init_task_manager` and never reallocates — `len` and the
///    backing pointer are stable.
/// 2. `Option<KBox<Task>>` has niche-optimised layout (one pointer;
///    null = None). Stores of the pointer are atomic on x86_64 so a
///    reader observes either null (skip) or a valid KBox pointer.
/// 3. Under the "KBoxes live forever" rule, once a slot is `Some`,
///    that pointer is valid until kernel shutdown. The Task contents
///    may cycle through identities, but `task_id` is a naturally
///    aligned u32 (atomic u32 load on x86_64), yielding either the
///    current live ID, `INVALID_TASK_ID`, or a recent stale ID —
///    all benign for the caller.
pub fn task_find_by_id(task_id: u32) -> *mut Task {
    if task_id == INVALID_TASK_ID {
        return ptr::null_mut();
    }

    // SAFETY: lock-free read — see function doc for invariants.
    let mgr_ptr = unsafe { &*TASK_MANAGER.as_ptr() };
    let bound = pool_high_water().min(mgr_ptr.tasks.len());
    for slot in &mgr_ptr.tasks[..bound] {
        if let Some(kbox) = slot.as_deref() {
            if kbox.task_id == task_id {
                return kbox as *const Task as *mut Task;
            }
        }
    }
    ptr::null_mut()
}

/// Find a live task whose active address space matches `cr3`.
///
/// This is primarily used by exception paths that need to recover the faulting
/// task even if per-CPU scheduler current-task metadata is temporarily stale.
/// **Lock-free** — see [`task_find_by_id`] for the safety argument.
pub fn task_find_by_cr3(cr3: u64) -> *mut Task {
    if cr3 == 0 {
        return ptr::null_mut();
    }

    let target = cr3 & !0xFFF;
    // SAFETY: lock-free read — same rationale as task_find_by_id.
    let mgr_ptr = unsafe { &*TASK_MANAGER.as_ptr() };
    let bound = pool_high_water().min(mgr_ptr.tasks.len());
    let mut fallback: *mut Task = ptr::null_mut();

    for slot in &mgr_ptr.tasks[..bound] {
        let Some(kbox) = slot.as_deref() else {
            continue;
        };
        let status = kbox.status();
        if status == TaskStatus::Invalid || status == TaskStatus::Terminated {
            continue;
        }

        let task_cr3 =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(kbox.context.cr3)) } & !0xFFF;
        if task_cr3 != target {
            continue;
        }

        let task_ptr = kbox as *const Task as *mut Task;
        if status == TaskStatus::Running {
            return task_ptr;
        }

        if fallback.is_null() {
            fallback = task_ptr;
        }
    }

    fallback
}

/// Look up the pool index of `task` in `mgr`.
///
/// Returns `Some(idx)` when `task` is a live pool-member, `None`
/// otherwise. The fast path reads `task.slot_index` (an O(1) field
/// populated by [`reserve_task_slot`]) and validates it against the
/// pool; a linear scan fallback handles edge cases where `slot_index`
/// is the `u32::MAX` sentinel or out of range.
pub(super) fn task_slot_index_inner(mgr: &TaskManagerInner, task: *const Task) -> Option<usize> {
    if task.is_null() {
        return None;
    }
    // Fast path: Task's own slot_index field.
    let hint = unsafe { (*task).slot_index } as usize;
    if hint < mgr.tasks.len() {
        if let Some(kbox) = mgr.tasks[hint].as_deref() {
            if (kbox as *const Task) == task {
                return Some(hint);
            }
        }
    }
    // Fallback: linear scan bounded by the high-water mark — no need
    // to look past slots that have never held a Task.
    let bound = pool_high_water().min(mgr.tasks.len());
    for (i, slot) in mgr.tasks[..bound].iter().enumerate() {
        if let Some(kbox) = slot.as_deref() {
            if (kbox as *const Task) == task {
                return Some(i);
            }
        }
    }
    None
}

pub fn task_pointer_is_valid(task: *const Task) -> bool {
    with_task_manager(|mgr| task_slot_index_inner(mgr, task).is_some())
}

pub(super) enum ReserveTaskSlotError {
    MaxTasks,
    NoFreeSlot,
}

/// Reserve a pool slot for a new task.
///
/// Three-tier scan under the lock:
/// 1. Reuse an existing `Some(kbox)` whose Task is `Invalid`.
/// 2. Reuse an existing `Some(kbox)` whose Task is `Terminated` and
///    whose refcount has dropped to zero (resets in place).
/// 3. Allocate a fresh `KBox<Task>` (via `init_invalid` recipe, no
///    stack rvalue) and install it in a `None` slot.
///
/// On success: sets the Task's status to `Blocked` to close the TOCTOU
/// race with a second concurrent caller, populates `slot_index`, and
/// returns `(*mut Task, task_id)`.
pub(super) fn reserve_task_slot() -> Result<(*mut Task, u32), ReserveTaskSlotError> {
    // Lazy-init guard: APs may reserve an idle task before the
    // Services-phase `init_task_manager` step runs (see
    // `ensure_pool_allocated` for the ordering rationale).
    if !ensure_pool_allocated() {
        return Err(ReserveTaskSlotError::NoFreeSlot);
    }
    with_task_manager(|mgr| {
        let capacity = mgr.tasks.len();
        if mgr.num_tasks as usize >= capacity {
            return Err(ReserveTaskSlotError::MaxTasks);
        }

        let hwm = pool_high_water().min(capacity);
        let mut chosen_idx: Option<usize> = None;

        // Tier 1: Some(kbox) with Invalid status — only among slots
        // that have actually been populated.
        for (i, slot) in mgr.tasks[..hwm].iter().enumerate() {
            if let Some(kbox) = slot.as_deref() {
                if kbox.status() == TaskStatus::Invalid {
                    chosen_idx = Some(i);
                    break;
                }
            }
        }

        // Tier 2: Some(kbox) with Terminated+refcnt=0 — reset and reuse.
        if chosen_idx.is_none() {
            for (i, slot) in mgr.tasks[..hwm].iter_mut().enumerate() {
                if let Some(kbox) = slot.as_deref_mut() {
                    if kbox.status() == TaskStatus::Terminated && kbox.ref_count() == 0 {
                        // SAFETY: exclusive &mut under the manager lock.
                        unsafe { Task::reset_in_place(kbox as *mut Task) };
                        chosen_idx = Some(i);
                        break;
                    }
                }
            }
        }

        // Tier 3: allocate a fresh KBox past the high-water mark.
        // The first `None` slot is guaranteed to sit at index `hwm` —
        // every slot below has been populated at least once and never
        // reverts to `None` — so there's no need to scan for one.
        let need_fresh = chosen_idx.is_none();
        if need_fresh {
            if hwm >= capacity {
                return Err(ReserveTaskSlotError::NoFreeSlot);
            }
            let i = hwm;
            let kbox = match KBox::try_init(Task::init_invalid()) {
                Ok(b) => b,
                Err(_) => return Err(ReserveTaskSlotError::NoFreeSlot),
            };
            mgr.tasks[i] = Some(kbox);
            bump_pool_high_water(i);
            chosen_idx = Some(i);
        }

        let idx = chosen_idx.expect("tier 1/2/3 must produce a slot");

        let slot_ptr: *mut Task = {
            let kbox = mgr.tasks[idx]
                .as_deref_mut()
                .expect("chosen slot must be Some");
            // TOCTOU protection: publish Blocked under the lock so no
            // concurrent caller can reserve the same slot.
            kbox.set_status(TaskStatus::Blocked);
            kbox.slot_index = idx as u32;
            kbox as *mut Task
        };

        mgr.exit_records[idx] = TaskExitRecord::empty();

        let task_id = mgr.next_task_id;
        mgr.next_task_id = task_id.wrapping_add(1);
        mgr.num_tasks += 1;

        Ok((slot_ptr, task_id))
    })
}

/// Release a previously reserved task slot back to Invalid.
/// Called when task_create fails after reserve_task_slot succeeded.
pub(super) fn release_task_slot(slot: *mut Task) {
    if slot.is_null() {
        return;
    }
    with_task_manager(|mgr| {
        unsafe { Task::reset_in_place(slot) };
        mgr.num_tasks = mgr.num_tasks.saturating_sub(1);
    });
}

pub fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let task = task_find_by_id(task_id);
    unsafe {
        if task.is_null() || (*task).status() == TaskStatus::Invalid {
            *task_info = ptr::null_mut();
            return -1;
        }
        *task_info = task;
    }
    0
}

pub fn task_get_exit_record(task_id: u32, record_out: *mut TaskExitRecord) -> c_int {
    if record_out.is_null() {
        return -1;
    }
    with_task_manager(|mgr| {
        for rec in mgr.exit_records.iter() {
            if rec.task_id == task_id {
                unsafe { *record_out = *rec };
                return 0;
            }
        }
        -1
    })
}

pub fn task_get_current_id() -> u32 {
    let current = scheduler::scheduler_get_current_task();
    if current.is_null() {
        0
    } else {
        unsafe { (*current).task_id }
    }
}

pub fn task_get_current() -> *mut Task {
    scheduler::scheduler_get_current_task()
}

pub fn task_set_current(task: *mut Task) {
    if task.is_null() {
        return;
    }
    unsafe {
        let current_status = (*task).status();
        if current_status != TaskStatus::Ready && current_status != TaskStatus::Running {
            klog_info!(
                "task_set_current: unexpected state {} for task {} ('{}')",
                current_status.as_u8() as u32,
                (*task).task_id,
                bytes_as_str(&(*task).name)
            );
        }
        (*task).set_status(TaskStatus::Running);
    }
}

pub fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    let cb = match callback {
        Some(cb) => cb,
        None => return,
    };

    // Collect active task pointers into a heap-backed KVec so the
    // caller's stack frame doesn't carry a multi-KiB array of
    // pointers, and we can release the manager lock before running
    // callbacks (which may take other locks transitively).
    //
    // Size the scratch buffer to the current pool high-water mark,
    // not the full pool capacity — the HWM bounds how many slots can
    // possibly be populated, so a larger capacity would just waste
    // heap on signal-hot paths.
    let hwm = pool_high_water().max(1);
    let mut addrs = match KVec::<usize>::with_capacity(hwm) {
        Ok(v) => v,
        Err(_) => return,
    };
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks_mut() {
            if task.status() != TaskStatus::Invalid && task.task_id != INVALID_TASK_ID {
                let _ = addrs.push(task as *mut Task as usize);
            }
        }
    });

    for addr in addrs.iter() {
        cb(*addr as *mut Task, context);
    }
}

/// Return a breakdown of slot states across the pool.
///
/// Returns `(num_tasks, none_or_invalid, terminated, active)`.
/// `none_or_invalid` counts slots that are either `None` (never
/// allocated) or `Some(kbox)` with `status == Invalid` — both are
/// reusable by `reserve_task_slot`.
pub fn task_slot_census() -> (u32, u32, u32, u32) {
    with_task_manager(|mgr| {
        let hwm = pool_high_water().min(mgr.tasks.len());
        let never_populated = (mgr.tasks.len() - hwm) as u32;
        let mut none_or_invalid = never_populated;
        let mut terminated = 0u32;
        let mut active = 0u32;
        for slot in &mgr.tasks[..hwm] {
            match slot.as_deref() {
                None => none_or_invalid += 1,
                Some(kbox) => match kbox.status() {
                    TaskStatus::Invalid => none_or_invalid += 1,
                    TaskStatus::Terminated => terminated += 1,
                    _ => active += 1,
                },
            }
        }
        (mgr.num_tasks, none_or_invalid, terminated, active)
    })
}

pub unsafe fn task_manager_force_unlock() {
    unsafe { TASK_MANAGER.force_unlock() };
}

/// Force-unlock the task manager AND mark it as poisoned.
/// Called from panic recovery to signal that the task table may be
/// in an inconsistent state. The next `init_task_manager()` call
/// clears the poison after reinitialising invariants.
pub unsafe fn task_manager_poison_unlock() {
    unsafe { TASK_MANAGER.poison_unlock() };
}
