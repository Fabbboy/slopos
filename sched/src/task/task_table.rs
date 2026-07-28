use core::ffi::c_int;
use core::ops::{ControlFlow, Deref};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::handle::Handle;
use slopos_ostd::string::bytes_as_str;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::task::{task_existence_park, task_existence_release};
use slopos_ostd::{KArc, KVec, KWeak};
use slopos_ostd::{klog_debug, klog_info};

use super::{INVALID_TASK_ID, MAX_TASKS, Task, TaskStatus, task_put};
use crate::exit_info::ExitInfo;
use crate::scheduler;

/// The kernel's owning task handle — outside OSTD, the only one.
///
/// Every way to obtain a strong task reference lands here: the registry's
/// liveness-checked weak upgrade, a placement container giving its reference
/// back, a clone minted from a live one, a task surrendering its existence
/// reference. Each is a constructor below, and none of them hands out the
/// `KArc<Task>` underneath.
///
/// That is deliberate, and it is the whole safety argument. `Task`'s destructor
/// frees to the buddy allocator, whose reuse path waits on synchronous
/// cross-CPU TLB drains, so whether it may run *here* is a question about the
/// calling context — asked by [`super::task_put`], which `Drop` below routes
/// every release through. `KArc`'s own `Drop` does not ask it. So a bare
/// `KArc<Task>` in a binding is a hazard rather than a handle: every scope exit
/// is a release site, including the ones `?` and `return` create, and a struct
/// field holding one has a derived `Drop` that is silently the wrong one. Since
/// no expression outside this module produces a `KArc<Task>`, that mistake is
/// not merely discouraged — it cannot be written.
///
/// The raw pointer escape ([`Self::as_ptr`]) is transitional scheduler plumbing
/// and remains valid for exactly the lifetime of this guard (or of another
/// owning reference).
pub struct TaskRef {
    arc: Option<KArc<Task>>,
}

impl TaskRef {
    #[inline]
    pub(super) fn new(arc: KArc<Task>) -> Self {
        Self { arc: Some(arc) }
    }

    /// Take back the reference a placement container parked, as a guard.
    ///
    /// # Correctness
    /// Inherits `task_placement_reclaim`'s contract: `node` must name a
    /// reference this container parked and has not yet reclaimed.
    #[inline]
    pub(crate) fn from_placement(node: NonNull<Task>) -> Self {
        Self::new(slopos_ostd::task::task_placement_reclaim(node))
    }

    /// Mint a second guard onto a task some other owner is keeping alive.
    ///
    /// # Correctness
    /// Inherits `task_placement_clone`'s contract: the caller must hold, or be
    /// covered by, a live strong reference to `node` for the duration.
    #[inline]
    pub(crate) fn clone_of(node: NonNull<Task>) -> Self {
        Self::new(slopos_ostd::task::task_placement_clone(node))
    }

    /// Take back a task's own existence reference, as a guard.
    ///
    /// `None` when another reaper already won it, which is what makes a reap
    /// idempotent. Same liveness contract as [`Self::clone_of`].
    #[inline]
    pub(crate) fn take_existence(node: NonNull<Task>) -> Option<Self> {
        task_existence_release(node).map(Self::new)
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut Task {
        KArc::as_ptr(self.arc.as_ref().expect("live TaskRef")) as *mut Task
    }

    /// This task's allocation node, for the placement primitives.
    ///
    /// [`Deref`] already answers "read this task's state"; this answers the
    /// other question a live guard gets asked — "park a reference to it" — for
    /// the callers that mint one (a ready-queue publication, a wait map, a
    /// futex bucket). Handing out the node rather than the handle is what keeps
    /// `task.arc().clone()` from being a way back to a bare `KArc<Task>`, while
    /// still carrying the mint's precondition — *the caller holds a live strong
    /// reference* — as a fact in the signature rather than a comment.
    #[inline]
    pub fn node(&self) -> NonNull<Task> {
        KArc::node(self.arc.as_ref().expect("live TaskRef"))
    }

    /// Park this guard's reference in a raw slot, yielding the node pointer to
    /// store. The inverse of [`Self::from_placement`], and the only way a guard
    /// leaves the type system — into a slot that hands it straight back.
    #[inline]
    pub(crate) fn into_placement(self) -> NonNull<Task> {
        slopos_ostd::task::task_placement_leak(self.into_arc())
    }

    /// Surrender the handle this guard wraps.
    ///
    /// `pub(super)` on purpose: this is the one place a bare `KArc<Task>`
    /// escapes the guard, and its only caller is the release path in
    /// [`super::task_reclaim`], which consumes it immediately. Widening this is
    /// what would put an un-guarded handle back into a binding.
    ///
    /// Costs no refcount traffic — the guard is only ever a wrapper around this
    /// one reference.
    #[inline]
    pub(super) fn into_arc(mut self) -> KArc<Task> {
        self.arc.take().expect("live TaskRef")
    }

    /// Wrap a freshly built, never-registered task so the reclaim tests can
    /// exercise a release that really is final.
    ///
    /// The one constructor that takes a handle rather than a node, and gated to
    /// `test-hooks` for exactly that reason: a production caller reaching for it
    /// would be a production caller holding a bare `KArc<Task>`.
    #[cfg(feature = "test-hooks")]
    pub fn from_arc_for_test(arc: KArc<Task>) -> Self {
        Self::new(arc)
    }

    /// Weak handle onto the same allocation, so a test can distinguish "the
    /// registration is gone" from "the allocation is gone".
    #[cfg(feature = "test-hooks")]
    pub fn downgrade_for_test(&self) -> KWeak<Task> {
        KArc::downgrade(self.arc.as_ref().expect("live TaskRef"))
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
        // Routed through the universal release rather than dropped: a guard on a
        // reaped task can hold the final reference, and guards are constructed
        // and dropped *under* the registry cli-spinlock (the cr3 scan, every
        // registry walk, the fixture reset). The release decrements here and
        // defers the allocator-heavy destructor to a context that can run it.
        //
        // This is why the guard exists at all, and why nothing hands out the
        // handle it wraps: `KArc<Task>`'s own `Drop` skips that decision.
        super::task_reclaim::release_arc(arc);
    }
}

/// One registered task.
///
/// The registry never owns a task. `weak` is the only handle it keeps, so a
/// lookup is a liveness-checked upgrade and there is no way to fabricate a
/// strong reference from an entry. What keeps a registered task alive instead is
/// its own existence reference: a task is registered exactly while it holds one,
/// and the reap gives it back and unhashes the entry as a single step.
struct RegistryEntry {
    id: u32,
    weak: KWeak<Task>,
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

    /// Whether `task` addresses one of this registry's entries.
    ///
    /// Compares against the weak handle's identity address, which is defined
    /// without upgrading and without reading through the pointer — upgrading
    /// here would mint a handle that could become the final reference and run
    /// the allocator-heavy destructor under this lock. Because a registered task
    /// is exactly one holding its existence reference, a match still means the
    /// pointer names a live, initialised task.
    fn owns_pointer(&self, task: *const Task) -> bool {
        self.slots[..self.high_water]
            .iter()
            .flatten()
            .any(|entry| core::ptr::eq(entry.get().weak.as_ptr(), task))
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

/// Reap a task: unhash its registration and give back the existence reference
/// it has held since registration, as one step.
///
/// This is SlopOS's `release_task`. It declines while the task is still
/// dispatch-pinned, because unhashing a task that is still some CPU's current
/// would make [`task_pointer_is_valid`] report pointer corruption and send
/// `scheduler_tasks_for_cpu` down its recovery path on the dying task's own
/// stack; the deferred drain retries once the pin clears.
///
/// The gate is a statement about task *state*, never about a reference count: a
/// count pre-check cannot be made race-free, and the final release is decided by
/// the decrement inside [`super::task_put`] instead.
fn reap_task_registration(id: u32) -> bool {
    if id == INVALID_TASK_ID {
        return false;
    }
    let taken = with_task_manager(|mgr| {
        // A registered task holds its existence reference, so this upgrade
        // cannot fail for an entry that is present.
        //
        // Held as a guard rather than as the bare handle: every exit below
        // releases it while the registry cli-spinlock is held, and one of them
        // is reached exactly when a racing reaper already took the existence
        // reference — which makes this upgrade the last one. `TaskRef::drop`
        // routes that through `task_put`, so the allocator-heavy destructor
        // never runs under the lock.
        let task = TaskRef::new(mgr.registry.find(id)?.weak.upgrade()?);
        let node = NonNull::new(task.as_ptr())?;
        if task.status() != TaskStatus::Terminated {
            return None;
        }
        if crate::scheduler::task_is_dispatch_pinned(&task) {
            REAP_BLOCKED_BY_DISPATCH.store(true, Ordering::Release);
            return None;
        }
        let existence = TaskRef::take_existence(node)?;
        // Unhash while the existence reference is still held: the weak count is
        // then at least two (the implicit one the strongs own, plus this entry's),
        // so dropping the entry is a bare decrement that provably cannot reach
        // the allocator from under this cli-spinlock.
        let entry = mgr.registry.remove(id).expect("found under the same lock");
        drop(entry.weak);
        // The temporary upgrade above. Non-final while `existence` is held.
        task_put(task);
        Some(existence)
    });
    let Some(existence) = taken else {
        return false;
    };
    // Off-lock, so this may be the final release and run the destructor inline.
    task_put(existence);
    true
}

/// Retire a registration unconditionally, ignoring the status and dispatch-pin
/// gates. Fixture reset only.
///
/// Unlinks the task from its parent first: a zombie still parked in a preserved
/// parent's children list would otherwise keep a reference in a list nothing can
/// reach afterwards, leaving the task linked, unreachable and never dropped.
fn force_reap_registration(id: u32) {
    let Some(guard) = task_find_by_id(id) else {
        return;
    };
    let Some(node) = NonNull::new(guard.as_ptr()) else {
        return;
    };
    // Off-lock, and before the unhash: resolving the parent takes the same lock.
    if let Some(parent_ref) = super::unlink_child(node.as_ptr()) {
        task_put(parent_ref);
    }
    let existence = TaskRef::take_existence(node);
    let entry = with_task_manager(|mgr| mgr.registry.remove(id));
    if let Some(entry) = entry {
        slopos_ostd::task::drop_off_lock(entry.weak);
    }
    drop(guard);
    if let Some(existence) = existence {
        task_put(existence);
    }
}

/// One-shot retry latch for reaps refused because the task was still
/// dispatch-pinned. The idle dispatcher drains it via
/// [`task_reap_dispatch_pinned`]; ids are never recorded, because id-keyed
/// re-lookup is race-free and allocation-free — recording them would mean
/// allocating under the registry lock, which the spine design forbids.
static REAP_BLOCKED_BY_DISPATCH: AtomicBool = AtomicBool::new(false);

/// Whether a reap has been refused for a still-pinned task since the last drain.
#[inline]
pub fn task_reap_pending() -> bool {
    REAP_BLOCKED_BY_DISPATCH.load(Ordering::Acquire)
}

/// Arm the deferred-reap latch directly, for paths that move a task to
/// `Terminated` but cannot reap it themselves.
#[inline]
pub fn arm_deferred_reap() {
    REAP_BLOCKED_BY_DISPATCH.store(true, Ordering::Release);
}

/// Reap terminated tasks whose reap was refused while they were dispatch-pinned.
///
/// Called by the idle dispatcher with interrupts enabled and no lock held; a
/// no-op unless a refusal armed the latch since the last drain.
pub fn task_reap_dispatch_pinned() {
    if !REAP_BLOCKED_BY_DISPATCH.swap(false, Ordering::AcqRel) {
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
        REAP_BLOCKED_BY_DISPATCH.store(true, Ordering::Release);
    }
    for id in &ids[..count] {
        let _ = reap_task_registration(*id);
    }
}

/// Reap the task `id` names, if it is ready to be reaped.
///
/// The sole entry point for teardown paths: `task_terminate`'s cleanup, the
/// post-switch cleanup of a task that has just left its CPU, `waitpid`'s reap,
/// and a dying parent's auto-reap of its zombie children.
#[inline]
pub(crate) fn task_reap(id: u32) -> bool {
    reap_task_registration(id)
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
            if crate::per_cpu::is_idle_task(slopos_ostd::task::TaskAddr::of(&task)) {
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
        force_reap_registration(*id);
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
    // Retiring the previous generation's tasks above may have parked their
    // final references; destroy them here so a test fixture never starts with
    // the previous run's corpses outstanding.
    super::task_graveyard_drain();
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
            // `task` is the registry guard the walk yields; it derefs to
            // `&Task`, so the racy context read comes off it directly.
            let task_cr3 = task.context_cr3() & !0xFFF;
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
    crate::safestack_rt::is_bootstrap_task_ptr(task.cast())
}

pub(super) enum TaskAllocError {
    MaxTasks,
    NoFreeSlot,
    IdExhausted,
}

/// Sole ownership of a task that is being built and is not yet reachable.
///
/// The token *is* the pre-publication window: while it exists the task has no
/// registry entry, so no lookup, no active-task walk and no diagnostic scan can
/// observe it half-constructed, and the only way to reach it is [`Self::as_mut`]
/// — an exclusive borrow the compiler can see. Construction therefore finishes
/// before the task becomes reachable, which is what makes the field writes need
/// no witness: there is nobody to be a witness against.
///
/// Every exit is accounted for. [`register_task`] consumes the token and hands
/// back the strong reference that pins the now-registered task; dropping it
/// instead gives the reservation back and releases the allocation.
pub struct PendingTask {
    task: Option<KArc<Task>>,
    id: u32,
}

impl PendingTask {
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Exclusive access to the task being built.
    ///
    /// The exclusivity is *checked*, not asserted: the token holds the only
    /// strong reference and the registry has not published a weak one yet, so
    /// `KArc::get_mut` succeeds precisely when nobody else can reach the
    /// allocation. The `&mut self` receiver is
    /// what carries that fact out to the caller — a second builder cannot
    /// exist, and the borrow ends before `register_task` consumes the token.
    #[inline]
    pub fn as_mut(&mut self) -> &mut Task {
        KArc::get_mut(self.task.as_mut().expect("pending task owns allocation"))
            .expect("pending task is the sole reference until registration")
    }
}

impl Drop for PendingTask {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        with_task_manager(|mgr| mgr.num_tasks = mgr.num_tasks.saturating_sub(1));
        // Always the final release — the token holds the only reference a
        // never-registered task ever had — so it goes through the universal
        // release like every other one. `drop_off_lock` checks interrupts and
        // the lock count but not the preempt guard, and an abandon under one
        // would run the destructor in the very context `task_put` defers.
        task_put(TaskRef::new(task));
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

/// Publish a fully initialized task into the registry and hand it its own
/// existence reference.
///
/// The registry keeps only a weak handle, so what makes the task outlive this
/// call is the existence reference — parked after the insert succeeds, so a
/// rejected insert leaves nothing to unpark. Fails only when every spine slot is
/// occupied (live tasks plus not-yet-reaped terminated ones); the caller then
/// discards the pending task.
///
/// The returned guard pins the task across the rest of the caller's own
/// construction, which still works through a raw pointer. It is a [`TaskRef`]
/// rather than the bare handle so that the construction path releases through
/// [`super::task_put`] like every other holder: the callers that take it run a
/// fallible tail (`CLONE_*_SETTID` writes, `publish_new_task`) whose error exits
/// terminate the child first, and that makes the scope-end drop the child's
/// final release.
pub(super) fn register_task(mut pending: PendingTask) -> Result<TaskRef, PendingTask> {
    let id = pending.id;
    let task = pending.task.take().expect("pending task owns allocation");
    let Some(node) = NonNull::new(KArc::as_ptr(&task) as *mut Task) else {
        pending.task = Some(task);
        return Err(pending);
    };
    let entry = RegistryEntry {
        id,
        weak: KArc::downgrade(&task),
    };
    let rejected = with_task_manager(|mgr| {
        debug_assert!(mgr.registry.find(id).is_none(), "task id collision");
        mgr.registry.insert(entry).err()
    });
    match rejected {
        None => {
            let parked = task_existence_park(node);
            debug_assert!(
                parked,
                "a freshly registered task already held its existence reference"
            );
            Ok(TaskRef::new(task))
        }
        Some(entry) => {
            slopos_ostd::task::drop_off_lock(entry.weak);
            pending.task = Some(task);
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

pub fn task_consume_zombie(task_id: u32) -> Option<ExitInfo> {
    let task = task_find_by_id(task_id)?;
    if task.status() != TaskStatus::Zombie {
        return None;
    }
    let info = task.exit_info.try_get().cloned()?;
    if !task.try_transition_to(TaskStatus::Terminated) {
        return None;
    }
    let child_ptr = task.as_ptr();
    // waitpid drops the parent's owning reference off-lock: unlink the child
    // from its parent's children list and release the reference the list held.
    // A Zombie is pinned by that reference, so without this the reaped child
    // would stay Terminated-pinned until the parent itself exits.
    //
    // The lookup guard stays live across the unlink. The transition above made
    // the child reapable, so from that instant a peer CPU's deferred-reap drain
    // may retire the registration and hand back the existence reference; this
    // guard is then the only thing keeping `child_ptr` addressable.
    if let Some(parent_ref) = super::unlink_child(child_ptr) {
        task_put(parent_ref);
    }
    drop(task);
    let _ = reap_task_registration(task_id);
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
    scheduler::current_task_id()
}

/// Visit every registered task that is neither `Invalid` nor id-less.
///
/// The guards are upgraded into a snapshot under the registry lock and `f` runs
/// only after that lock is released, so a visitor may take the registry lock
/// again — resolve a parent, enqueue a task, post a signal — without
/// deadlocking. Each guard pins its task for the whole visit.
pub fn task_for_each_active(mut f: impl FnMut(&TaskRef)) {
    task_try_for_each_active(|task| {
        f(task);
        ControlFlow::Continue(())
    });
}

/// [`task_for_each_active`] with early exit: visiting stops as soon as `f`
/// returns [`ControlFlow::Break`].
///
/// Visitors are lent the [`TaskRef`] guard rather than a bare `&Task`. Most
/// only read, and [`Deref`] makes that spelling identical — but the walk also
/// feeds the stranded-task rescue, which has to *park* a reference on a ready
/// queue, and a shared borrow is not proof of a non-zero strong count. Lending
/// the guard is what lets that one caller mint a reference without the walk
/// handing every other caller a raw pointer to do it with.
pub fn task_try_for_each_active(mut f: impl FnMut(&TaskRef) -> ControlFlow<()>) {
    // The buffer is sized in one lock section and filled in another, so the
    // registry can gain an entry in between. `KVec::with_capacity` reserves
    // exactly, so an overflowing `push` would reallocate *under* the registry
    // cli-spinlock — the buddy's reuse drain is a cross-CPU wait, and
    // allocating under a cli-lock is the known slab/LUF deadlock. So the fill
    // never grows the buffer: it reports how many entries it saw and the retry
    // re-reserves off-lock. Truncating instead would silently drop tasks from a
    // walk that feeds the stranded-task rescue and the shutdown sweep. The spine
    // holds at most `MAX_TASKS`, so this converges.
    let mut capacity = with_task_manager(|mgr| mgr.registry.len()).max(1);
    let tasks = loop {
        let mut tasks = match KVec::<TaskRef>::with_capacity(capacity) {
            Ok(tasks) => tasks,
            Err(_) => return,
        };
        let seen = with_task_manager(|mgr| {
            let mut seen = 0usize;
            for task in mgr.iter_tasks() {
                if task.status() == TaskStatus::Invalid || task.task_id == INVALID_TASK_ID {
                    continue;
                }
                seen += 1;
                if seen <= capacity {
                    let _ = tasks.push(task);
                }
            }
            seen
        });
        if seen <= capacity {
            break tasks;
        }
        // Off-lock, so releasing the partial snapshot may destroy inline.
        drop(tasks);
        capacity = seen;
    };
    for task in tasks.iter() {
        if f(task).is_break() {
            return;
        }
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
    let mut ring = match task.take_test_reports() {
        Some(ring) => ring,
        None => return KVec::new(),
    };
    ring.drain().unwrap_or_else(|_| KVec::new())
}

pub fn debug_dump_tasks_klog() {
    klog_info!("SYSRQ: ---- task dump ----");
    task_for_each_active(|guard| dump_one_task(guard));
    klog_info!("SYSRQ: ---- end task dump ----");
}

fn dump_one_task(t: &Task) {
    let reason = t.load_block_reason();
    let placement = t.sched_placement();
    let on_cpu = t.on_cpu();
    let last_run = t.last_run_timestamp();
    klog_info!(
        "SYSRQ: task {:>3} '{}' status={:?} reason={:?} placement={:?} on_cpu={} pid={} pgid={} sid={} last_run={}",
        t.task_id,
        bytes_as_str(&t.name),
        t.status(),
        reason,
        placement,
        on_cpu,
        t.process_id,
        t.pgid(),
        t.sid(),
        last_run,
    );
    if t.status() == TaskStatus::Blocked {
        let (ctx_rip, ctx_rsp) = t.switch_ctx_rip_rsp();
        let ctx_rbp = t.switch_ctx_rbp();
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
