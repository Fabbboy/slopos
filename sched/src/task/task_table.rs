use core::ffi::{c_int, c_void};
use core::ops::Deref;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::handle::Handle;
use slopos_ostd::string::bytes_as_str;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_REGISTRY, SpinLock, held_lock_count};
use slopos_ostd::{KArc, KVec, KWeak};
use slopos_ostd::{klog_debug, klog_info};

use super::task_accessors::task_id_of;
use super::{INVALID_TASK_ID, MAX_TASKS, Task, TaskIterateCb, TaskStatus};
use crate::exit_info::ExitInfo;
use crate::scheduler;

/// Strong task reference returned by registry lookups.
///
/// The registry stores only `KWeak<Task>`; constructing this guard is the one
/// liveness-checked weak upgrade path. The raw pointer escape is transitional
/// scheduler plumbing and remains valid for exactly the lifetime of this
/// guard (or of another owning reference).
pub struct TaskRef {
    arc: Option<KArc<Task>>,
}

impl TaskRef {
    #[inline]
    fn new(arc: KArc<Task>) -> Self {
        Self { arc: Some(arc) }
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut Task {
        KArc::as_ptr(self.arc.as_ref().expect("live TaskRef")) as *mut Task
    }
}

impl Clone for TaskRef {
    fn clone(&self) -> Self {
        Self::new(self.arc.as_ref().expect("live TaskRef").clone())
    }
}

impl Deref for TaskRef {
    type Target = Task;

    fn deref(&self) -> &Task {
        self.arc.as_ref().expect("live TaskRef")
    }
}

impl PartialEq for TaskRef {
    fn eq(&self, other: &Self) -> bool {
        KArc::ptr_eq(
            self.arc.as_ref().expect("live TaskRef"),
            other.arc.as_ref().expect("live TaskRef"),
        )
    }
}

impl Eq for TaskRef {}

impl Drop for TaskRef {
    fn drop(&mut self) {
        let Some(arc) = self.arc.take() else {
            return;
        };
        let id = arc.task_id;
        let terminated = arc.status() == TaskStatus::Terminated;
        drop(arc);

        // A lookup may be the last strong reference besides the temporary
        // scheduler-lifetime owner. Retire that owner (or, from contexts in
        // which Task's allocator-heavy destructor may not run, arm the idle
        // dispatcher's deferred retry).
        if terminated {
            let _ = task_try_reclaim_id(id);
        }
    }
}

/// One registered task. Lookups upgrade `weak`; `owner` is the transitional
/// scheduler-lifetime strong reference, held here until the scheduler's
/// placement containers take ownership, after which the registry is
/// weak-only.
struct RegistryEntry {
    id: u32,
    weak: KWeak<Task>,
    owner: KArc<Task>,
}

/// Weak-upgrade liveness index over a pre-reserved slot spine.
///
/// IDs are never recycled, so `RegistryEntry::id` is the stable identity and
/// no parallel slot-generation scheme exists — array slots are reused, IDs
/// are not. The spine is allocated once outside the manager lock
/// ([`ensure_registry_allocated`]) and never grows: every mutation under the
/// cli-spinlock is a plain slot write, so registration and retirement never
/// touch the heap while the lock is held (the buddy's LUF reuse drain is a
/// hidden cross-CPU wait; allocating under a cli-lock is the known
/// slab/LUF deadlock).
struct TaskRegistry {
    slots: KVec<Option<KernelSync<RegistryEntry>>>,
    /// Occupied-slot count.
    live: usize,
    /// Monotone scan bound: one past the highest slot ever occupied, so
    /// lookups walk O(peak concurrent tasks), not O(capacity).
    high_water: usize,
    /// First-free search hint; insertion scans circularly from here.
    free_hint: usize,
}

impl TaskRegistry {
    const fn new() -> Self {
        Self {
            slots: KVec::new(),
            live: 0,
            high_water: 0,
            free_hint: 0,
        }
    }

    fn is_allocated(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Adopt a pre-filled spine. Returns the spine back to the caller (for
    /// an off-lock drop) when a racing allocation already installed one.
    fn install(
        &mut self,
        spine: KVec<Option<KernelSync<RegistryEntry>>>,
    ) -> Option<KVec<Option<KernelSync<RegistryEntry>>>> {
        if self.is_allocated() {
            return Some(spine);
        }
        self.slots = spine;
        None
    }

    fn find(&self, id: u32) -> Option<&RegistryEntry> {
        if id == INVALID_TASK_ID {
            return None;
        }
        self.slots[..self.high_water]
            .iter()
            .flatten()
            .map(KernelSync::get)
            .find(|entry| entry.id == id)
    }

    fn get(&self, id: u32) -> Option<TaskRef> {
        self.find(id)?.weak.upgrade().map(TaskRef::new)
    }

    /// Store `entry` in a free slot. On a full (or uninstalled) spine the
    /// entry is handed back so the caller can drop its handles off-lock.
    fn insert(&mut self, entry: RegistryEntry) -> Result<(), RegistryEntry> {
        let capacity = self.slots.len();
        if capacity == 0 || self.live == capacity {
            return Err(entry);
        }
        let start = self.free_hint.min(capacity - 1);
        let mut idx = start;
        let free = loop {
            if self.slots[idx].is_none() {
                break Some(idx);
            }
            idx = (idx + 1) % capacity;
            if idx == start {
                break None;
            }
        };
        let Some(idx) = free else {
            return Err(entry);
        };
        self.slots[idx] = Some(KernelSync::new(entry));
        self.live += 1;
        self.high_water = self.high_water.max(idx + 1);
        self.free_hint = (idx + 1) % capacity;
        Ok(())
    }

    /// Move an entry out of its slot. The caller owns the returned handles
    /// and must drop them off-lock; the slot itself is reused in place.
    fn remove(&mut self, id: u32) -> Option<RegistryEntry> {
        if id == INVALID_TASK_ID {
            return None;
        }
        let idx = self.slots[..self.high_water]
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|entry| entry.get().id == id))?;
        let entry = self.slots[idx].take()?;
        self.live -= 1;
        self.free_hint = idx;
        Some(entry.into_inner())
    }

    fn len(&self) -> usize {
        self.live
    }

    fn iter(&self) -> impl Iterator<Item = (u32, TaskRef)> + '_ {
        self.slots[..self.high_water]
            .iter()
            .flatten()
            .map(KernelSync::get)
            .filter_map(|entry| {
                entry
                    .weak
                    .upgrade()
                    .map(|arc| (entry.id, TaskRef::new(arc)))
            })
    }

    fn owns_pointer(&self, task: *const Task) -> bool {
        self.slots[..self.high_water]
            .iter()
            .flatten()
            .any(|entry| core::ptr::eq(KArc::as_ptr(&entry.get().owner), task))
    }
}

pub(super) struct TaskManagerInner {
    registry: TaskRegistry,
    pub(super) num_tasks: u32,
    /// Monotonic, non-wrapping allocator. The stored/public ID remains `u32`;
    /// exhaustion is a permanent allocation failure rather than reuse.
    pub(super) next_task_id: u64,
    pub(super) total_context_switches: u64,
    pub(super) total_yields: u64,
    pub(super) tasks_created: u32,
    pub(super) tasks_terminated: u32,
    pub(super) initialized: bool,
}

impl TaskManagerInner {
    const fn new() -> Self {
        Self {
            registry: TaskRegistry::new(),
            num_tasks: 0,
            next_task_id: 1,
            total_context_switches: 0,
            total_yields: 0,
            tasks_created: 0,
            tasks_terminated: 0,
            initialized: false,
        }
    }

    pub(super) fn iter_tasks(&self) -> impl Iterator<Item = TaskRef> + '_ {
        self.registry.iter().map(|(_, task)| task)
    }

    pub(super) fn iter_tasks_mut(&mut self) -> impl Iterator<Item = TaskRef> + '_ {
        self.registry.iter().map(|(_, task)| task)
    }
}

static TASK_MANAGER: SpinLock<TaskManagerInner> =
    SpinLock::new(TaskManagerInner::new(), LOCK_LEVEL_REGISTRY);

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

#[inline]
pub(super) fn task_registry_len() -> usize {
    with_task_manager(|mgr| mgr.registry.len())
}

/// Allocate the registry spine outside the manager lock and install it if
/// absent. A spine that loses the install race is dropped off-lock.
fn ensure_registry_allocated() -> bool {
    if with_task_manager(|mgr| mgr.registry.is_allocated()) {
        return true;
    }
    let mut spine = match KVec::with_capacity(MAX_TASKS) {
        Ok(spine) => spine,
        Err(_) => return false,
    };
    for _ in 0..MAX_TASKS {
        if spine.push(None).is_err() {
            return false;
        }
    }
    let leftover = with_task_manager(|mgr| mgr.registry.install(spine));
    drop(leftover);
    true
}

#[inline]
fn task_drop_context_is_safe() -> bool {
    slopos_ostd::cpu::x86_64::interrupts::are_interrupts_enabled() && held_lock_count() == 0
}

fn drop_unregistered(entry: Option<(KWeak<Task>, KArc<Task>)>) {
    let Some((weak, owner)) = entry else {
        return;
    };
    slopos_ostd::task::drop_off_lock(weak);
    slopos_ostd::task::drop_off_lock(owner);
}

fn remove_registration(id: u32, require_sole_owner: bool) -> Option<(KWeak<Task>, KArc<Task>)> {
    with_task_manager(|mgr| {
        if require_sole_owner && KArc::strong_count(&mgr.registry.find(id)?.owner) != 1 {
            return None;
        }
        let entry = mgr.registry.remove(id)?;
        Some((entry.weak, entry.owner))
    })
}

/// Retire a terminated task once the legacy scheduler reference count and
/// every upgraded registry guard are gone.
pub(crate) fn task_try_reclaim(task: *mut Task) -> bool {
    let Some(id) = task_id_of(task) else {
        return false;
    };
    task_try_reclaim_id(id)
}

/// Drop one owning reference held by a transitional scheduler container (ready
/// queue, remote inbox, dispatch slot, or wait map) and retire a terminated
/// task when that drop leaves only the registry owner.
///
/// The id and termination status are captured before the drop: retirement may
/// run the owner's destructor, after which the pointer must not be touched.
/// The drop here is never the last strong reference — the registry owner
/// outlives every placement reference — so it is a bare atomic decrement and is
/// safe under a lock or with interrupts disabled; the actual destructor runs
/// off-lock inside [`task_try_reclaim_id`].
#[inline]
pub fn release_placement_arc(arc: KArc<Task>) {
    let id = arc.task_id;
    let terminated = arc.status() == TaskStatus::Terminated;
    drop(arc);
    if terminated {
        let _ = task_try_reclaim_id(id);
    }
}

/// Drop one owning placement reference and, for a terminated task, arm the idle
/// dispatcher's reclaim drain instead of retiring it inline.
///
/// The context-switch tail uses this so its drain stays a bare atomic decrement:
/// the allocator-heavy `Task` destructor (buddy free + cross-CPU TLB drain)
/// never runs in the switch window, only later on the idle stack via
/// [`task_reclaim_deferred`]. As with [`release_placement_arc`], this drop is
/// never the last strong reference — the registry owner outlives it.
#[inline]
pub fn release_placement_arc_deferred(arc: KArc<Task>) {
    let terminated = arc.status() == TaskStatus::Terminated;
    drop(arc);
    if terminated {
        arm_deferred_reclaim();
    }
}

/// One-shot retry latch for reclaims attempted from contexts where the
/// `Task` destructor may not run (IRQs off or a tracked lock held). The
/// idle dispatcher drains it via [`task_reclaim_deferred`]; ids are never
/// recorded because id-keyed re-lookup is race-free and allocation-free.
static RECLAIM_PENDING: AtomicBool = AtomicBool::new(false);

fn task_try_reclaim_id(id: u32) -> bool {
    if id == INVALID_TASK_ID {
        return false;
    }
    if !task_drop_context_is_safe() {
        RECLAIM_PENDING.store(true, Ordering::Release);
        return false;
    }
    let reclaimable = with_task_manager(|mgr| {
        let owner = &mgr.registry.find(id)?.owner;
        let task = owner.as_ref();
        if task.status() != TaskStatus::Terminated {
            return None;
        }
        if !task.on_cpu.load(Ordering::Acquire) && KArc::strong_count(owner) == 1 {
            return Some(());
        }
        // Terminated but still pinned. Every pinning reference retries on
        // release, but a releaser may have sampled the status before this
        // task turned Terminated — arm the idle drain so that race cannot
        // strand the task.
        RECLAIM_PENDING.store(true, Ordering::Release);
        None
    })
    .is_some();
    if !reclaimable {
        return false;
    }
    let entry = remove_registration(id, true);
    let removed = entry.is_some();
    drop_unregistered(entry);
    removed
}

/// Whether a deferred reclaim attempt has armed the retry latch since the
/// last [`task_reclaim_deferred`] drain.
#[inline]
pub fn task_reclaim_pending() -> bool {
    RECLAIM_PENDING.load(Ordering::Acquire)
}

/// Arm the deferred-reclaim latch directly. For paths that transition tasks
/// to `Terminated` but cannot perform the retirement themselves.
#[inline]
pub(super) fn arm_deferred_reclaim() {
    RECLAIM_PENDING.store(true, Ordering::Release);
}

/// Retire terminated tasks whose reclaim was attempted from a context that
/// could not run the destructor. Called by the idle dispatcher with IRQs
/// enabled and no tracked lock held; a no-op unless such an attempt armed
/// the latch since the last drain.
pub fn task_reclaim_deferred() {
    if !RECLAIM_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    const BATCH: usize = 32;
    let mut ids = [INVALID_TASK_ID; BATCH];
    let mut count = 0usize;
    let mut truncated = false;
    with_task_manager(|mgr| {
        for (id, task) in mgr.registry.iter() {
            if task.status() != TaskStatus::Terminated {
                continue;
            }
            if count == BATCH {
                truncated = true;
                break;
            }
            ids[count] = id;
            count += 1;
        }
    });
    if truncated {
        RECLAIM_PENDING.store(true, Ordering::Release);
    }
    for id in &ids[..count] {
        let _ = task_try_reclaim_id(*id);
    }
}

/// Initialize the registry. Reinitialization is a test-fixture operation:
/// preserve CPU idle tasks, retire every other registration, and keep the
/// monotonic ID source advancing so IDs are never reused across resets.
pub fn init_task_manager() -> c_int {
    if !ensure_registry_allocated() {
        return -1;
    }
    let was_initialized = with_task_manager(|mgr| mgr.initialized);
    if !was_initialized {
        with_task_manager(|mgr| mgr.initialized = true);
        TASK_MANAGER.clear_poison();
        crate::sleep::init_sleep_queue();
        return 0;
    }

    let mut retire = match KVec::with_capacity(MAX_TASKS) {
        Ok(ids) => ids,
        Err(_) => return -1,
    };
    with_task_manager(|mgr| {
        for (id, task) in mgr.registry.iter() {
            if crate::per_cpu::is_idle_task(task.as_ptr()) {
                klog_debug!(
                    "init_task_manager: preserving idle task {} ('{}')",
                    id,
                    bytes_as_str(&task.name)
                );
            } else if retire.push(id).is_err() {
                return false;
            }
        }
        true
    });
    for id in retire.iter() {
        drop_unregistered(remove_registration(*id, false));
    }
    with_task_manager(|mgr| {
        mgr.total_context_switches = 0;
        mgr.total_yields = 0;
        mgr.tasks_created = 0;
        mgr.tasks_terminated = 0;
        mgr.num_tasks = mgr.registry.len() as u32;
        mgr.initialized = true;
    });
    TASK_MANAGER.clear_poison();
    crate::sleep::init_sleep_queue();
    0
}

/// Whether `task_id` was ever handed out by the allocator. Because ids are
/// monotonic and never reused, any id below the watermark named a real task
/// at some point — it is now either live or fully retired — whereas an id at
/// or above the watermark never existed. Lets callers treat an operation on
/// an already-retired task as idempotent rather than "no such task".
pub fn task_id_was_allocated(task_id: u32) -> bool {
    if task_id == INVALID_TASK_ID || task_id == 0 {
        return false;
    }
    with_task_manager(|mgr| (task_id as u64) < mgr.next_task_id)
}

/// Find a task by its never-reused ID. The returned guard owns the successful
/// weak upgrade; absence and completed destruction both return `None`.
pub fn task_find_by_id(task_id: u32) -> Option<TaskRef> {
    if task_id == INVALID_TASK_ID {
        return None;
    }
    with_task_manager(|mgr| mgr.registry.get(task_id))
}

/// Raw projection for legacy test fixtures whose own scoped handle pins the
/// task before retaining the pointer.
#[cfg(feature = "test-hooks")]
pub fn task_find_by_id_raw_for_test(task_id: u32) -> *mut Task {
    task_find_by_id(task_id).map_or(ptr::null_mut(), |task| task.as_ptr())
}

/// Mint the width-compatible task handle. Its slot component is the monotonic
/// task ID; generation is permanently zero because IDs are never recycled.
pub fn task_handle(task_id: u32) -> Option<Handle<Task>> {
    task_find_by_id(task_id).map(|_| Handle::from_parts(task_id, 0))
}

/// Resolve a task handle through the same weak-upgrade path as ID lookup.
pub fn task_resolve_handle(handle: Handle<Task>) -> Option<TaskRef> {
    if handle.generation() != 0 {
        return None;
    }
    task_find_by_id(handle.slot())
}

#[cfg(feature = "test-hooks")]
pub fn task_resolve_handle_raw_for_test(handle: Handle<Task>) -> *mut Task {
    task_resolve_handle(handle).map_or(ptr::null_mut(), |task| task.as_ptr())
}

/// Find a live task whose active address space matches `cr3`.
///
/// The registry cli-spinlock makes this safe in exception context; weak
/// upgrade performs no allocation. Callers must release the returned guard
/// before entering a diverging exception tail.
pub fn task_find_by_cr3(cr3: u64) -> Option<TaskRef> {
    if cr3 == 0 {
        return None;
    }
    let target = cr3 & !0xFFF;
    with_task_manager(|mgr| {
        let mut fallback = None;
        for task in mgr.iter_tasks() {
            let status = task.status();
            if matches!(status, TaskStatus::Invalid | TaskStatus::Terminated) {
                continue;
            }
            let task_cr3 =
                super::task_accessors::task_context_cr3(task.as_ptr()).unwrap_or(0) & !0xFFF;
            if task_cr3 != target {
                continue;
            }
            if status == TaskStatus::Running {
                return Some(task);
            }
            if fallback.is_none() {
                fallback = Some(task);
            }
        }
        fallback
    })
}

pub fn task_pointer_is_valid(task: *const Task) -> bool {
    if task.is_null() {
        return false;
    }
    if with_task_manager(|mgr| mgr.registry.owns_pointer(task)) {
        return true;
    }
    crate::safestack_rt::is_bootstrap_task_ptr(task)
}

pub(super) enum TaskAllocError {
    MaxTasks,
    NoFreeSlot,
    IdExhausted,
}

pub(super) struct PendingTask {
    task: Option<KArc<Task>>,
    id: u32,
}

impl PendingTask {
    #[inline]
    pub(super) fn id(&self) -> u32 {
        self.id
    }

    #[inline]
    pub(super) fn as_ptr(&self) -> *mut Task {
        KArc::as_ptr(self.task.as_ref().expect("pending task owns allocation")) as *mut Task
    }
}

impl Drop for PendingTask {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        with_task_manager(|mgr| mgr.num_tasks = mgr.num_tasks.saturating_sub(1));
        slopos_ostd::task::drop_off_lock(task);
    }
}

/// Reserve capacity and allocate one task without publishing it to lookups.
pub(super) fn allocate_task() -> Result<PendingTask, TaskAllocError> {
    if !ensure_registry_allocated() {
        return Err(TaskAllocError::NoFreeSlot);
    }
    let id = with_task_manager(|mgr| {
        if mgr.num_tasks as usize >= MAX_TASKS {
            return Err(TaskAllocError::MaxTasks);
        }
        // Registered-but-unreclaimed tasks (zombies awaiting waitpid) occupy
        // spine slots without counting toward `num_tasks`; refuse early so
        // registration after full initialization almost never fails.
        if mgr.registry.len() >= MAX_TASKS {
            return Err(TaskAllocError::NoFreeSlot);
        }
        if mgr.next_task_id >= INVALID_TASK_ID as u64 {
            return Err(TaskAllocError::IdExhausted);
        }
        let id = mgr.next_task_id as u32;
        mgr.next_task_id += 1;
        mgr.num_tasks += 1;
        Ok(id)
    })?;

    let mut task = match KArc::try_init(Task::init_invalid()) {
        Ok(task) => task,
        Err(_) => {
            with_task_manager(|mgr| mgr.num_tasks = mgr.num_tasks.saturating_sub(1));
            return Err(TaskAllocError::NoFreeSlot);
        }
    };
    let value = KArc::get_mut(&mut task).expect("fresh task allocation must be unique");
    value.task_id = id;
    value.set_status(TaskStatus::Blocked);
    Ok(PendingTask {
        task: Some(task),
        id,
    })
}

/// Publish a fully initialized task into the registry: the weak handle
/// serves lookups, the strong handle is the transitional scheduler-lifetime
/// owner. Fails only when every spine slot is occupied (live tasks plus
/// not-yet-retired terminated ones); the caller discards the pending task.
pub(super) fn register_task(mut pending: PendingTask) -> Result<*mut Task, PendingTask> {
    let id = pending.id;
    let task = pending.task.take().expect("pending task owns allocation");
    let raw = KArc::as_ptr(&task) as *mut Task;
    let entry = RegistryEntry {
        id,
        weak: KArc::downgrade(&task),
        owner: task,
    };
    let rejected = with_task_manager(|mgr| {
        debug_assert!(mgr.registry.find(id).is_none(), "task id collision");
        mgr.registry.insert(entry).err()
    });
    match rejected {
        None => Ok(raw),
        Some(entry) => {
            slopos_ostd::task::drop_off_lock(entry.weak);
            pending.task = Some(entry.owner);
            Err(pending)
        }
    }
}

#[cfg(feature = "test-hooks")]
pub fn task_live_cap_rejects_for_test() -> bool {
    let (saved_live, saved_next, saved_entries) = with_task_manager(|mgr| {
        let snapshot = (mgr.num_tasks, mgr.next_task_id, mgr.registry.len());
        mgr.num_tasks = MAX_TASKS as u32;
        snapshot
    });
    let result = allocate_task();
    let unchanged = with_task_manager(|mgr| {
        let unchanged = mgr.next_task_id == saved_next && mgr.registry.len() == saved_entries;
        mgr.num_tasks = saved_live;
        unchanged
    });
    matches!(result, Err(TaskAllocError::MaxTasks)) && unchanged
}

/// Abandon a task whose construction failed before publication.
pub(super) fn discard_task(pending: PendingTask) {
    drop(pending);
}

pub fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let Some(task) = task_find_by_id(task_id) else {
        slopos_ostd::util::ptr_buf::nullable_write(task_info, ptr::null_mut());
        return -1;
    };
    let raw = task.as_ptr();
    if task.status() == TaskStatus::Invalid {
        slopos_ostd::util::ptr_buf::nullable_write(task_info, ptr::null_mut());
        return -1;
    }
    slopos_ostd::util::ptr_buf::nullable_write(task_info, raw);
    0
}

pub fn task_consume_zombie(task_id: u32) -> Option<ExitInfo> {
    let task = task_find_by_id(task_id)?;
    if task.status() != TaskStatus::Zombie {
        return None;
    }
    let info = task.exit_info.try_get().cloned()?;
    if !task.try_transition_to(TaskStatus::Terminated) {
        return None;
    }
    drop(task);
    let _ = task_try_reclaim_id(task_id);
    Some(info)
}

pub fn task_peek_exit_info(task_id: u32) -> Option<ExitInfo> {
    let task = task_find_by_id(task_id)?;
    if matches!(task.status(), TaskStatus::Zombie | TaskStatus::Terminated) {
        task.exit_info.try_get().cloned()
    } else {
        None
    }
}

pub fn task_get_current_id() -> u32 {
    let current = scheduler::scheduler_get_current_task();
    task_id_of(current).unwrap_or(0)
}

pub fn task_get_current() -> *mut Task {
    scheduler::scheduler_get_current_task()
}

pub fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    let Some(cb) = callback else {
        return;
    };
    let capacity = with_task_manager(|mgr| mgr.registry.len()).max(1);
    let mut tasks = match KVec::<TaskRef>::with_capacity(capacity) {
        Ok(tasks) => tasks,
        Err(_) => return,
    };
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.status() != TaskStatus::Invalid && task.task_id != INVALID_TASK_ID {
                let _ = tasks.push(task);
            }
        }
    });
    for task in tasks.iter() {
        cb(task.as_ptr(), context);
    }
}

/// Return `(live, remaining_capacity, terminated, active)` for diagnostics.
pub fn task_slot_census() -> (u32, u32, u32, u32) {
    with_task_manager(|mgr| {
        let mut terminated = 0u32;
        let mut active = 0u32;
        for task in mgr.iter_tasks() {
            if task.status() == TaskStatus::Terminated {
                terminated += 1;
            } else if task.status() != TaskStatus::Invalid {
                active += 1;
            }
        }
        (
            mgr.num_tasks,
            (MAX_TASKS as u32).saturating_sub(mgr.num_tasks),
            terminated,
            active,
        )
    })
}

pub fn task_drain_test_reports(task_id: u32) -> KVec<crate::test_reports::TestReport> {
    let Some(task) = task_find_by_id(task_id) else {
        return KVec::new();
    };
    let mut ring = match super::task_accessors::task_take_test_reports(task.as_ptr()) {
        Some(ring) => ring,
        None => return KVec::new(),
    };
    ring.drain().unwrap_or_else(|_| KVec::new())
}

pub fn debug_dump_tasks_klog() {
    klog_info!("SYSRQ: ---- task dump ----");
    task_iterate_active(Some(dump_one_task), ptr::null_mut());
    klog_info!("SYSRQ: ---- end task dump ----");
}

fn dump_one_task(task: *mut Task, _context: *mut c_void) {
    let Some(t) = super::task_borrow(task) else {
        return;
    };
    let reason = slopos_ostd::task::accessors::task_load_block_reason(task as *const Task);
    let placement = slopos_ostd::task::accessors::task_sched_placement_load(task as *const Task);
    let on_cpu = slopos_ostd::task::accessors::task_on_cpu_load(task as *const Task);
    let last_run =
        slopos_ostd::task::accessors::task_last_run_timestamp(task as *const Task).unwrap_or(0);
    klog_info!(
        "SYSRQ: task {:>3} '{}' status={:?} reason={:?} placement={:?} on_cpu={} pid={} pgid={} sid={} last_run={}",
        t.task_id,
        bytes_as_str(&t.name),
        t.status(),
        reason,
        placement,
        on_cpu,
        t.process_id,
        t.pgid,
        t.sid,
        last_run,
    );
    if t.status() == TaskStatus::Blocked {
        let (ctx_rip, ctx_rsp) =
            slopos_ostd::task::accessors::task_switch_ctx_rip_rsp(task as *const Task)
                .unwrap_or((0, 0));
        let ctx_rbp =
            slopos_ostd::task::accessors::task_switch_ctx_rbp(task as *const Task).unwrap_or(0);
        klog_info!(
            "SYSRQ:   parked at rip=0x{:x} rsp=0x{:x} rbp=0x{:x}",
            ctx_rip,
            ctx_rsp,
            ctx_rbp
        );
        if ctx_rbp != 0 {
            let mut entries: [slopos_ostd::stacktrace::StacktraceEntry; 12] =
                [slopos_ostd::stacktrace::StacktraceEntry {
                    frame_pointer: 0,
                    return_address: 0,
                }; 12];
            let captured = slopos_ostd::stacktrace::stacktrace_capture_from(
                ctx_rbp,
                entries.as_mut_ptr(),
                entries.len() as core::ffi::c_int,
            );
            for entry in entries.iter().take(captured.max(0) as usize) {
                klog_info!("SYSRQ:   frame rip=0x{:x}", entry.return_address);
            }
        }
    }
}
