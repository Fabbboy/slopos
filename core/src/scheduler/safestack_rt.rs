//! SafeStack-sanitizer runtime.
//!
//! Provides the two primitives that let the LLVM SafeStack pass
//! (`-Zsanitizer=safestack -C llvm-args=-safestack-use-pointer-address`)
//! drive per-task unsafe (data) stacks on our SMP kernel:
//!
//! 1. [`__safestack_pointer_address`] — naked C-ABI function LLVM's
//!    instrumented prologues call to fetch the slot address.  Returns
//!    `&current_task->unsafe_stack_sp`.
//! 2. Bootstrap Task stubs — per-CPU pre-allocated `Task` structs
//!    with `unsafe_stack_sp` primed to a dedicated bootstrap stack
//!    buffer.  Installed into each CPU's `PCR.current_task` *before*
//!    any instrumented Rust runs on that CPU.
//!
//! # Why the slot lives in the Task, not the PCR
//!
//! LLVM's `safestack-use-pointer-address` mode caches the pointer
//! returned from the callback on the safe stack (emitted as a spill
//! `movq %rax, 16(%rsp)`) and reuses it across multiple loads / stores
//! inside the same function.  If the slot address were per-CPU (e.g.
//! `&gs:[PCR::UNSAFE_SP]`, which evaluates to a concrete virtual
//! address on one specific CPU's PCR page), the cached pointer would
//! become **stale** the moment the scheduler migrates the task to
//! another CPU — the task resumes on the new CPU but its cached
//! pointer still names the old CPU's unsafe-SP slot.
//! Reads/writes through the stale pointer hit the old CPU's PCR —
//! which belongs to whichever task is scheduled there now —
//! corrupting both.
//!
//! Embedding the slot inside the `Task` struct — heap-allocated,
//! addresses stable for the lifetime of the allocation — makes the
//! cached pointer inherently task-local.  Migration becomes safe by
//! construction: the slot *moves with the task* because it IS the
//! task, and the pointer keeps pointing at the same field no matter
//! which CPU the task runs on.
//!
//! Fuchsia avoids the race differently: their custom `x86_64-fuchsia`
//! LLVM target emits direct `%gs:UNSAFE_SP_OFFSET` loads on each
//! access, so there is never a cached pointer to go stale.  We get
//! equivalent correctness without the toolchain fork by shifting the
//! slot into the task.
//!
//! # Bootstrap order
//!
//! 1. `boot/limine_entry.s` (BSP) and the `ap_entry` naked trampoline
//!    in `boot/src/smp.rs` (APs) populate their own `PCR.self_ref`,
//!    their own `PCR.current_task`, and the referenced bootstrap
//!    Task's `unsafe_stack_sp` before issuing `wrmsr IA32_GS_BASE`.
//! 2. First instrumented Rust in `kernel_main` / `ap_entry_rust` calls
//!    `__safestack_pointer_address`, gets the bootstrap stub's
//!    `unsafe_stack_sp` field address, walks its SP down into the
//!    bootstrap stack buffer.
//! 3. Once the scheduler dispatches the idle task,
//!    `PCR.current_task` is updated; subsequent prologues start
//!    writing the real idle task's `unsafe_stack_sp`.  The bootstrap
//!    stub is never used again.

use core::cell::SyncUnsafeCell;

use slopos_ostd::sync::KernelSync;

use super::task::task_set_unsafe_stack_sp;
use super::task_struct::{TASK_UNSAFE_STACK_SP_OFFSET, Task};

/// Maximum number of statically-allocated AP bootstrap stubs.
/// Matches `slopos_arch::pcr::MAX_STATIC_APS`.
pub const MAX_STATIC_APS: usize = 16;

/// Size of each bootstrap unsafe-stack buffer — 64 KiB.  Ample for
/// any early-boot instrumented prologues on the path to
/// `kernel_main` / `ap_entry_rust`.
pub const BOOTSTRAP_UNSAFE_STACK_SIZE: usize = 0x10000;

/// Linker-visible copy of [`TASK_UNSAFE_STACK_SP_OFFSET`] — the
/// trampoline asm in `boot/limine_entry.s` and the AP naked trampoline
/// need the value to stamp each bootstrap Task's `unsafe_stack_sp`
/// field in asm before any instrumented Rust runs.  A Rust `const`
/// alone is invisible to the linker; this `static` round-trips the
/// value through a BSS-resident symbol the asm can `mov` against.
///
/// Declared `#[used]` so dead-code elimination keeps it even when the
/// Rust-side `TASK_UNSAFE_STACK_SP_OFFSET` is already inlined
/// everywhere.
#[used]
#[unsafe(no_mangle)]
pub static BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64 = TASK_UNSAFE_STACK_SP_OFFSET as u64;

// ---------------------------------------------------------------------------
// Bootstrap unsafe-stack buffers (raw BSS)
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
pub struct BootstrapUnsafeStack(pub [u8; BOOTSTRAP_UNSAFE_STACK_SIZE]);

#[unsafe(no_mangle)]
pub static BOOTSTRAP_UNSAFE_STACK: SyncUnsafeCell<BootstrapUnsafeStack> =
    SyncUnsafeCell::new(BootstrapUnsafeStack([0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]));

#[repr(C, align(16))]
pub struct ApBootstrapUnsafeStacks(pub [[u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS]);

#[unsafe(no_mangle)]
pub static APS_BOOTSTRAP_UNSAFE_STACKS: SyncUnsafeCell<ApBootstrapUnsafeStacks> =
    SyncUnsafeCell::new(ApBootstrapUnsafeStacks(
        [[0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS],
    ));

// ---------------------------------------------------------------------------
// Bootstrap Task stubs
// ---------------------------------------------------------------------------
//
// Each stub is a full `Task::invalid()` — ~6 KiB BSS per CPU.
// The only field that matters during bootstrap is `unsafe_stack_sp`,
// which gets primed to the top of the corresponding bootstrap unsafe
// stack before the CPU's GS_BASE is installed.  Every other field
// keeps its `Task::invalid()` default.

/// Bootstrap-Task stub cell; `Sync` via `KernelSync<T>`.
#[repr(transparent)]
pub struct BootstrapTaskCell(pub KernelSync<SyncUnsafeCell<Task>>);

impl BootstrapTaskCell {
    #[inline]
    pub const fn new(task: Task) -> Self {
        Self(KernelSync::new(SyncUnsafeCell::new(task)))
    }

    #[inline]
    pub fn get(&self) -> *mut Task {
        (*self.0).get()
    }
}

#[repr(transparent)]
pub struct BootstrapTaskArrayCell(pub KernelSync<SyncUnsafeCell<[Task; MAX_STATIC_APS]>>);

impl BootstrapTaskArrayCell {
    #[inline]
    pub const fn new(tasks: [Task; MAX_STATIC_APS]) -> Self {
        Self(KernelSync::new(SyncUnsafeCell::new(tasks)))
    }

    #[inline]
    pub fn get(&self) -> *mut [Task; MAX_STATIC_APS] {
        (*self.0).get()
    }

    /// Pointer to the `i`-th element of the bootstrap-task array.
    /// Computed via pointer arithmetic without dereferencing; the
    /// returned pointer is the same address `&raw mut (*self.get())[i]`
    /// would produce, but the cast is centralised here.
    #[inline]
    pub fn ptr_at(&self, i: usize) -> *mut Task {
        debug_assert!(i < MAX_STATIC_APS);
        let base: *mut Task = (*self.0).get().cast();
        base.wrapping_add(i)
    }
}

#[unsafe(no_mangle)]
pub static BSP_BOOTSTRAP_TASK: BootstrapTaskCell = BootstrapTaskCell::new(Task::invalid());

#[unsafe(no_mangle)]
pub static AP_BOOTSTRAP_TASKS: BootstrapTaskArrayCell = BootstrapTaskArrayCell::new({
    const INIT: Task = Task::invalid();
    [INIT; MAX_STATIC_APS]
});

// ---------------------------------------------------------------------------
// LLVM SafeStack callback
// ---------------------------------------------------------------------------
//
// `__safestack_pointer_address` is the naked LLVM-callback that
// returns `&current_task->abi.unsafe_stack_sp`. It lives in
// [`slopos_ostd::arch::x86_64::naked::__safestack_pointer_address`]
// — the naked-fn attribute stays inside OSTD; the kernel side just
// imports the behavioural contract via the symbol.
//
// The `TASK_UNSAFE_STACK_SP_OFFSET` constant in this module's `use`
// list is a re-export of [`slopos_ostd::task::abi::TASK_UNSAFE_STACK_SP_OFFSET`];
// it stays in this file as a convenience re-export so the asm in
// `boot/limine_entry.s` can resolve `BOOTSTRAP_TASK_UNSAFE_SP_OFFSET`
// without depending directly on the OSTD path.

// ---------------------------------------------------------------------------
// Bootstrap seeding helpers
// ---------------------------------------------------------------------------

/// Compute the top of the BSP bootstrap unsafe stack.
///
/// The end-of-buffer is a one-past-the-end pointer used only for
/// stack-top arithmetic; it is never dereferenced as the top itself,
/// so `wrapping_add` (a safe `const fn`) suffices.
#[inline]
pub fn bsp_bootstrap_unsafe_sp() -> *mut u8 {
    let base = BOOTSTRAP_UNSAFE_STACK.get() as *mut u8;
    let top = base.wrapping_add(BOOTSTRAP_UNSAFE_STACK_SIZE);
    // Align down to 16 bytes for x86-64 System V stack alignment.
    ((top as usize) & !0xF) as *mut u8
}

/// Compute the top of AP slot `i`'s bootstrap unsafe stack
/// (0-based: slot `i` corresponds to the AP whose `cpu_info.extra`
/// was set to `i + 1`).
#[inline]
pub fn ap_bootstrap_unsafe_sp(i: usize) -> *mut u8 {
    debug_assert!(i < MAX_STATIC_APS);
    let base = APS_BOOTSTRAP_UNSAFE_STACKS.get() as *mut u8;
    let top_off = (i + 1) * BOOTSTRAP_UNSAFE_STACK_SIZE;
    let top = base.wrapping_add(top_off);
    ((top as usize) & !0xF) as *mut u8
}

/// Seed every bootstrap Task stub with a valid `unsafe_stack_sp`.
/// Safe to call once on the BSP before any AP is started.
///
/// Pre-SMP single-writer phase: must run exactly once before any AP
/// trampoline reads `current_task->unsafe_stack_sp`.
pub fn init_bootstrap_tasks() {
    let bsp_task = BSP_BOOTSTRAP_TASK.get();
    task_set_unsafe_stack_sp(bsp_task, bsp_bootstrap_unsafe_sp() as u64);

    for i in 0..MAX_STATIC_APS {
        let ap_task = AP_BOOTSTRAP_TASKS.ptr_at(i);
        task_set_unsafe_stack_sp(ap_task, ap_bootstrap_unsafe_sp(i) as u64);
    }
}

/// Return `true` if `ptr` is one of the statically-allocated
/// bootstrap Task stubs (BSP or any AP).  Used by
/// `task_pointer_is_valid` to whitelist stubs alongside
/// pool-allocated tasks — the scheduler's pre-first-dispatch
/// window legitimately observes a stub as `PCR.current_task`, and
/// corruption-recovery paths would otherwise flag it as invalid
/// and loop trying to replace it with idle.
pub fn is_bootstrap_task_ptr(ptr: *const Task) -> bool {
    if ptr.is_null() {
        return false;
    }
    let p = ptr as usize;
    // BSP stub: exactly one Task.
    if p == BSP_BOOTSTRAP_TASK.get() as usize {
        return true;
    }
    // AP stubs: contiguous array of MAX_STATIC_APS Tasks.
    let base = AP_BOOTSTRAP_TASKS.get() as usize;
    let task_size = core::mem::size_of::<Task>();
    let end = base + task_size * MAX_STATIC_APS;
    if p >= base && p < end && (p - base) % task_size == 0 {
        return true;
    }
    false
}

/// Return a slice of raw `*mut ()` pointers to every AP bootstrap
/// task — suitable for passing to
/// `slopos_arch::pcr::init_ap_pcr_lookup`.  Each entry is the
/// bootstrap stub for AP slot `i + 1` (0-based index into the
/// returned slice).
///
/// Caller must not race with [`init_bootstrap_tasks`]; the returned
/// pointers are valid for the lifetime of the kernel image (BSS
/// globals never move).
pub fn ap_bootstrap_task_ptrs() -> [*mut (); MAX_STATIC_APS] {
    let mut out = [core::ptr::null_mut::<()>(); MAX_STATIC_APS];
    for i in 0..MAX_STATIC_APS {
        out[i] = AP_BOOTSTRAP_TASKS.ptr_at(i) as *mut ();
    }
    out
}
