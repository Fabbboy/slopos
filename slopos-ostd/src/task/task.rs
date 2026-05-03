//! Bare kernel `Task` primitive.
//!
//! Hosts the OSTD task type. The existing `core::scheduler::Task`
//! continues to drive execution while consumers are migrated; this
//! type compiles but is unwired. The fields mirror what the scheduler
//! needs:
//!
//! - [`TaskId`] / `generation` for stable identity + stale-handle detection.
//! - [`TaskContext`] for callee-saved-register snapshot, layout-compatible
//!   with `core::scheduler::SwitchContext` so the assembly in
//!   [`super::switch`] is byte-identical.
//! - [`KernelStack`] for owned kernel-mode stack pages (RAII-released on drop).
//! - `vm_space` for the address space the task runs in.
//! - [`FpuState`](super::fpu::FpuState) heap-allocated via
//!   `KBox::try_init` to keep the 2.6 KiB rvalue off the caller's stack.
//! - `is_running` enforces Inv. 8: a task is on at most one CPU at a time.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, offset_of};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::mm::vm_space::VmSpace;
use crate::mm::{KArc, KBox};
use crate::task::fpu::FpuState;

// ---------------------------------------------------------------------------
// Identity.
// ---------------------------------------------------------------------------

/// Stable kernel task identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TaskId(pub u64);

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

impl TaskId {
    /// Allocate a fresh, never-before-returned task ID.
    #[inline]
    pub fn alloc() -> Self {
        Self(NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// TaskContext.
// ---------------------------------------------------------------------------

/// Callee-saved register snapshot for software context switch.
///
/// Layout-compatible with `core::scheduler::SwitchContext`. The
/// assembly in [`super::switch`] reads/writes these offsets directly via
/// `offset_of!`, so the field order must not be changed without
/// updating the asm in lockstep.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskContext {
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub rip: u64,
}

impl TaskContext {
    /// All-zero context with `rflags` set to a sensible default
    /// (IF=1, IOPL=0).
    pub const fn zero() -> Self {
        Self {
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rflags: 0x202,
            rip: 0,
        }
    }

    /// Build a context that, when first dispatched via
    /// [`super::switch::switch_registers`], resumes at `trampoline`
    /// with `entry_point` in `r12` and `arg` in `r13`. The trampoline
    /// is expected to call `entry_point(arg)` and then invoke the
    /// task-exit hook.
    pub const fn new_for_task(entry_point: u64, arg: u64, stack_top: u64, trampoline: u64) -> Self {
        Self {
            rbx: 0,
            r12: entry_point,
            r13: arg,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: stack_top - 8,
            rflags: 0x202,
            rip: trampoline,
        }
    }
}

const _: () = assert!(core::mem::size_of::<TaskContext>() == 72);
const _: () = assert!(offset_of!(TaskContext, rbx) == 0);
const _: () = assert!(offset_of!(TaskContext, r12) == 8);
const _: () = assert!(offset_of!(TaskContext, r13) == 16);
const _: () = assert!(offset_of!(TaskContext, r14) == 24);
const _: () = assert!(offset_of!(TaskContext, r15) == 32);
const _: () = assert!(offset_of!(TaskContext, rbp) == 40);
const _: () = assert!(offset_of!(TaskContext, rsp) == 48);
const _: () = assert!(offset_of!(TaskContext, rflags) == 56);
const _: () = assert!(offset_of!(TaskContext, rip) == 64);

// ---------------------------------------------------------------------------
// KernelStack.
// ---------------------------------------------------------------------------

/// RAII-owned kernel-mode stack region.
///
/// Models the storage shape but does not yet allocate from the OSTD
/// frame allocator — kernel-stack provisioning still flows through
/// `mm::stack_va`. [`KernelStack::from_raw`] adopts an existing stack
/// region owned by the caller; [`Drop`] is currently a no-op.
pub struct KernelStack {
    base: usize,
    size: usize,
}

impl KernelStack {
    /// Adopt an existing stack region. The caller retains responsibility
    /// for deallocation; OSTD does not free the region on drop yet.
    ///
    /// # Safety
    ///
    /// `base` must point to the lowest byte of a stack region of at
    /// least `size` bytes. The region must remain valid for the
    /// lifetime of the resulting `KernelStack`.
    pub const unsafe fn from_raw(base: usize, size: usize) -> Self {
        Self { base, size }
    }

    /// Lowest address in the stack region.
    #[inline]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Size of the stack region in bytes.
    #[inline]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Highest address in the stack region (one past the end).
    #[inline]
    pub const fn top(&self) -> usize {
        self.base + self.size
    }
}

// ---------------------------------------------------------------------------
// Task.
// ---------------------------------------------------------------------------

/// The bare kernel task primitive.
///
/// Async state is intentionally absent — this is the synchronous task
/// shape only.
pub struct Task {
    id: TaskId,
    /// Generation counter — bumps every time this task slot is reused
    /// so wakers/handles can detect staleness.
    generation: u64,
    kernel_stack: KernelStack,
    ctx: UnsafeCell<TaskContext>,
    vm_space: Option<KArc<VmSpace>>,
    fpu_state: KBox<FpuState>,
    /// Inv. 8 — a task is on at most one CPU at a time.
    is_running: AtomicBool,
}

// SAFETY: `ctx` is only mutated by the scheduler under serialisation
// (the running-task invariant); `is_running` synchronises CPU dispatch.
// `KArc<VmSpace>` and `KBox<FpuState>` are both `Send`.
unsafe impl Send for Task {}
// SAFETY: see Send. Sync is required because the scheduler stores tasks
// behind `KArc<Task>`.
unsafe impl Sync for Task {}

impl Task {
    /// Build a fresh task that, when first dispatched, runs
    /// `entry(arg)`.
    ///
    /// # Safety
    ///
    /// - `kernel_stack` must be exclusively owned for the lifetime of
    ///   the task.
    /// - `entry` must be safe to call in kernel mode with `arg` as its
    ///   single argument.
    pub unsafe fn new(
        kernel_stack: KernelStack,
        vm_space: Option<KArc<VmSpace>>,
        entry: extern "sysv64" fn(arg: u64) -> !,
        arg: u64,
        trampoline: u64,
    ) -> Result<KArc<Self>, crate::mm::AllocError> {
        let stack_top = kernel_stack.top() as u64;
        let ctx = TaskContext::new_for_task(entry as u64, arg, stack_top, trampoline);
        let fpu_state = boxed_fpu_state()?;
        let task = Self {
            id: TaskId::alloc(),
            generation: 0,
            kernel_stack,
            ctx: UnsafeCell::new(ctx),
            vm_space,
            fpu_state,
            is_running: AtomicBool::new(false),
        };
        KArc::try_new(task)
    }

    /// This task's stable identifier.
    #[inline]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Generation counter — bumps every time the task slot is reused.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Pointer to the saved [`TaskContext`]. Used by the assembly in
    /// [`super::switch`] which expects a raw pointer.
    #[inline]
    pub fn context_ptr(&self) -> *mut TaskContext {
        self.ctx.get()
    }

    /// Borrow the kernel stack metadata.
    #[inline]
    pub fn kernel_stack(&self) -> &KernelStack {
        &self.kernel_stack
    }

    /// Borrow the address space, if any.
    #[inline]
    pub fn vm_space(&self) -> Option<&KArc<VmSpace>> {
        self.vm_space.as_ref()
    }

    /// Pointer to the FPU save area.
    #[inline]
    pub fn fpu_state_ptr(&self) -> *mut FpuState {
        // The KBox owns a stable heap allocation; getting the raw
        // pointer through `&*self.fpu_state` returns the heap address.
        &*self.fpu_state as *const FpuState as *mut FpuState
    }

    /// True while the task is currently dispatched on a CPU.
    #[inline]
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Atomically claim this task for the current CPU. Returns `true`
    /// on success; `false` if another CPU has already claimed it.
    /// Enforces Inv. 8.
    #[inline]
    pub fn try_mark_running(&self) -> bool {
        self.is_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release the running flag. Called by the scheduler after the task
    /// is fully descheduled.
    #[inline]
    pub fn mark_not_running(&self) {
        self.is_running.store(false, Ordering::Release);
    }
}

// `Task` Drop is implicit: `KBox<FpuState>` returns the heap slot, the
// `Option<KArc<VmSpace>>` decrements the address-space refcount, and the
// remaining fields are plain data. Inv. 9.

fn boxed_fpu_state() -> Result<KBox<FpuState>, crate::mm::AllocError> {
    use crate::mm::AllocError;

    // SAFETY: the closure writes a fully-valid `FpuState` into `slot`
    // before returning `Ok(())`, satisfying `Init::__init`'s contract.
    // The inner `slot.write(...)` is `unsafe` and is covered by this
    // `init_from_closure` safety bracket — using `addr_of_mut!`-style
    // direct writes instead of nested `unsafe` blocks keeps the
    // unused-unsafe lint quiet under `-D warnings`.
    let init = unsafe {
        crate::mm::init_from_closure(|slot: *mut FpuState| -> Result<(), AllocError> {
            slot.write(FpuState::new());
            Ok(())
        })
    };
    KBox::<FpuState>::try_init::<AllocError>(init)
}

// ---------------------------------------------------------------------------
// CurrentTask.
// ---------------------------------------------------------------------------

/// `!Send` token representing "this CPU's currently running task" at
/// the moment it was minted via [`current`].
///
/// The token cannot escape the CPU it was created on (Inv. 8) — that's
/// enforced at the type level by `PhantomData<*const ()>`.
#[must_use = "if unused, the token is dropped immediately and serves no purpose"]
pub struct CurrentTask {
    _ne: PhantomData<*const ()>,
}

impl CurrentTask {
    fn new() -> Self {
        Self { _ne: PhantomData }
    }
}

// ---------------------------------------------------------------------------
// TaskRuntimeBackend — one-shot registration hook.
// ---------------------------------------------------------------------------

/// Hooks the OSTD task surface uses to talk to the kernel scheduler's
/// per-CPU current-task slot.
///
/// Until registered, [`current`] panics — the OSTD surface must not be
/// reached before the kernel scheduler is up.
///
/// # Safety
///
/// `current_task` must return a valid pointer to a [`Task`] owned by
/// the kernel scheduler (or null, indicating no current task —
/// [`current`] panics in that case).
pub unsafe trait TaskRuntimeBackend: Send + Sync + 'static {
    /// Pointer to the current CPU's running [`Task`], or null.
    fn current_task(&self) -> *const Task;
}

struct UnregisteredBackend;

// SAFETY: the unregistered backend always returns null.
unsafe impl TaskRuntimeBackend for UnregisteredBackend {
    fn current_task(&self) -> *const Task {
        core::ptr::null()
    }
}

static DEFAULT_BACKEND: UnregisteredBackend = UnregisteredBackend;

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn TaskRuntimeBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); reads happen after the flag is observed Acquire.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production task-runtime backend.
///
/// # Safety
///
/// `backend` must live for the static lifetime of the kernel.
pub unsafe fn register_task_runtime_backend(backend: &'static dyn TaskRuntimeBackend) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_task_runtime_backend called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(backend);
    }
}

#[inline]
fn task_runtime_backend() -> &'static dyn TaskRuntimeBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_task_runtime_backend`.
    unsafe { *(*BACKEND_SLOT.0.get()).as_ptr() }
}

/// Mint a [`CurrentTask`] token for this CPU's running task.
///
/// # Panics
///
/// Panics if no task-runtime backend has been registered, or if the
/// backend returns a null current-task pointer.
#[inline]
pub fn current() -> CurrentTask {
    let ptr = task_runtime_backend().current_task();
    assert!(
        !ptr.is_null(),
        "slopos_ostd::task::current() called with no current task"
    );
    CurrentTask::new()
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_task_runtime_for_test() {
    BACKEND_INSTALLED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_monotonic() {
        let a = TaskId::alloc();
        let b = TaskId::alloc();
        assert!(b.0 > a.0);
    }

    #[test]
    fn task_context_size_72() {
        assert_eq!(core::mem::size_of::<TaskContext>(), 72);
    }

    #[test]
    fn task_context_zero_has_default_rflags() {
        let ctx = TaskContext::zero();
        assert_eq!(ctx.rflags, 0x202);
    }

    #[test]
    fn task_context_new_for_task_layout() {
        let ctx = TaskContext::new_for_task(0xAAAA, 0xBBBB, 0x10000, 0xCCCC);
        assert_eq!(ctx.r12, 0xAAAA);
        assert_eq!(ctx.r13, 0xBBBB);
        assert_eq!(ctx.rsp, 0x10000 - 8);
        assert_eq!(ctx.rip, 0xCCCC);
        assert_eq!(ctx.rflags, 0x202);
    }

    #[test]
    #[should_panic(expected = "called with no current task")]
    fn current_panics_without_backend() {
        reset_task_runtime_for_test();
        let _ = current();
    }
}
