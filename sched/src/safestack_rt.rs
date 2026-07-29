//! SafeStack-sanitizer runtime.
//!
//! Provides the two primitives that let the LLVM SafeStack pass
//! (`-Zsanitizer=safestack -C llvm-args=-safestack-use-pointer-address`)
//! drive per-task data stacks on our SMP kernel:
//!
//! 1. [`__safestack_pointer_address`] — naked C-ABI function LLVM's
//!    instrumented prologues call to fetch the slot address.  Returns
//!    `&current_task->unsafe_stack_sp`.
//! 2. Bootstrap stubs — one per-CPU [`BootstrapTaskAbi`], the eight-byte
//!    prefix a `Task` shares with it, primed to a dedicated bootstrap
//!    stack buffer.  Installed into each CPU's `PCR.current_task`
//!    *before* any instrumented Rust runs on that CPU.
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
//! pointer still names the old CPU's data-SP slot.
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
use core::sync::atomic::{AtomicU64, Ordering};

use super::task_struct::TASK_UNSAFE_STACK_SP_OFFSET;

/// Maximum number of statically-allocated AP bootstrap stubs.
/// Matches `slopos_arch::pcr::MAX_STATIC_APS`.
pub const MAX_STATIC_APS: usize = 16;

/// Size of each bootstrap data-stack buffer — 64 KiB.  Ample for
/// any early-boot instrumented prologues on the path to
/// `kernel_main` / `ap_entry_rust`.
pub const BOOTSTRAP_UNSAFE_STACK_SIZE: usize = 0x10000;

slopos_ostd::no_mangle_static! {
    /// Linker-visible copy of [`TASK_UNSAFE_STACK_SP_OFFSET`] — the
    /// trampoline asm in `boot/limine_entry.s` and the AP naked
    /// trampoline need the value to stamp each bootstrap Task's
    /// `unsafe_stack_sp` field in asm before any instrumented Rust
    /// runs. A Rust `const` alone is invisible to the linker; this
    /// `static` round-trips the value through a BSS-resident symbol
    /// the asm can `mov` against.
    ///
    /// Declared `#[used]` so dead-code elimination keeps it even when
    /// the Rust-side `TASK_UNSAFE_STACK_SP_OFFSET` is already inlined
    /// everywhere.
    #[used]
    pub static BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64 = TASK_UNSAFE_STACK_SP_OFFSET as u64;
}

// ---------------------------------------------------------------------------
// Bootstrap data-stack buffers (raw BSS)
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
pub struct BootstrapUnsafeStack(pub [u8; BOOTSTRAP_UNSAFE_STACK_SIZE]);

slopos_ostd::no_mangle_static! {
    pub static BOOTSTRAP_UNSAFE_STACK: SyncUnsafeCell<BootstrapUnsafeStack> =
        SyncUnsafeCell::new(BootstrapUnsafeStack([0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]));
}

#[repr(C, align(16))]
pub struct ApBootstrapUnsafeStacks(pub [[u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS]);

slopos_ostd::no_mangle_static! {
    pub static APS_BOOTSTRAP_UNSAFE_STACKS: SyncUnsafeCell<ApBootstrapUnsafeStacks> =
        SyncUnsafeCell::new(ApBootstrapUnsafeStacks(
            [[0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS],
        ));
}

// ---------------------------------------------------------------------------
// Bootstrap Task stubs
// ---------------------------------------------------------------------------

/// The prefix of `Task` that pre-heap bootstrap needs, and nothing else.
///
/// A stub exists to answer exactly one question — "where is this CPU's data
/// stack pointer?" — asked by the SafeStack prologue as `gs:[CURRENT_TASK]`
/// then `[rax + TASK_UNSAFE_STACK_SP_OFFSET]`. That offset is zero, so eight
/// bytes suffice; a full `Task` body per CPU was ~8 KiB of `.bss` of which
/// every field but this one kept its `invalid()` default and was never read.
///
/// Being a distinct type is the point, not an optimisation. Nothing can spell a
/// stub as `*mut Task` any more, so the "PCR names a stub" case cannot reach a
/// task accessor and read eight bytes past the object. Both PCR readers filter
/// stubs out — `CurrentTask::get` by the `INVALID_TASK_ID` the stub is always
/// published with, `IdleTask::current` because the idle slot never holds one.
#[repr(C)]
pub struct BootstrapTaskAbi {
    /// Layout-identical to `TaskAbi::unsafe_stack_sp`. Atomic so the seeding
    /// path needs no `unsafe`; the boot asm's plain `mov` is a valid relaxed
    /// store against it on x86-64.
    pub unsafe_stack_sp: AtomicU64,
}

// The asm reaches this field through `TASK_UNSAFE_STACK_SP_OFFSET`, computed
// from `TaskAbi` inside OSTD. A stub must agree with a real task on it.
const _: () = assert!(core::mem::offset_of!(BootstrapTaskAbi, unsafe_stack_sp) == 0);
const _: () = assert!(TASK_UNSAFE_STACK_SP_OFFSET == 0);
const _: () = assert!(core::mem::size_of::<BootstrapTaskAbi>() == 8);

impl BootstrapTaskAbi {
    #[inline]
    const fn new() -> Self {
        Self {
            unsafe_stack_sp: AtomicU64::new(0),
        }
    }
}

/// Bootstrap-stub cell. `AtomicU64` supplies the interior mutability the
/// seeding path needs, so this is a plain `Sync` static with no cell wrapper.
#[repr(transparent)]
pub struct BootstrapTaskCell(pub BootstrapTaskAbi);

impl BootstrapTaskCell {
    #[inline]
    pub const fn new() -> Self {
        Self(BootstrapTaskAbi::new())
    }

    /// Address of this stub, in the PCR's own opaque shape.
    #[inline]
    pub fn get(&self) -> *mut () {
        core::ptr::from_ref(&self.0) as *mut ()
    }
}

#[repr(transparent)]
pub struct BootstrapTaskArrayCell(pub [BootstrapTaskAbi; MAX_STATIC_APS]);

impl BootstrapTaskArrayCell {
    #[inline]
    pub const fn new() -> Self {
        const INIT: BootstrapTaskAbi = BootstrapTaskAbi::new();
        Self([INIT; MAX_STATIC_APS])
    }

    #[inline]
    pub fn get(&self) -> *mut () {
        core::ptr::from_ref(&self.0) as *mut ()
    }

    /// Address of AP slot `i`, in the PCR's own opaque shape.
    #[inline]
    pub fn ptr_at(&self, i: usize) -> *mut () {
        debug_assert!(i < MAX_STATIC_APS);
        core::ptr::from_ref(&self.0[i]) as *mut ()
    }
}

slopos_ostd::no_mangle_static! {
    pub static BSP_BOOTSTRAP_TASK: BootstrapTaskCell = BootstrapTaskCell::new();
}

slopos_ostd::no_mangle_static! {
    pub static AP_BOOTSTRAP_TASKS: BootstrapTaskArrayCell = BootstrapTaskArrayCell::new();
}

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

/// Compute the top of the BSP bootstrap data stack.
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

/// Compute the top of AP slot `i`'s bootstrap data stack
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
    BSP_BOOTSTRAP_TASK
        .0
        .unsafe_stack_sp
        .store(bsp_bootstrap_unsafe_sp() as u64, Ordering::Release);

    for (i, stub) in AP_BOOTSTRAP_TASKS.0.iter().enumerate() {
        stub.unsafe_stack_sp
            .store(ap_bootstrap_unsafe_sp(i) as u64, Ordering::Release);
    }
}

/// Return `true` if `ptr` is one of the statically-allocated
/// bootstrap Task stubs (BSP or any AP). The scheduler's
/// pre-first-dispatch window legitimately observes a stub as
/// `PCR.current_task`, so the raw readers that still exist need a
/// way to tell one from a registry-owned task by address.
pub fn is_bootstrap_task_ptr(ptr: *const ()) -> bool {
    if ptr.is_null() {
        return false;
    }
    let p = ptr as usize;
    // BSP stub: exactly one.
    if p == BSP_BOOTSTRAP_TASK.get() as usize {
        return true;
    }
    // AP stubs: contiguous array of MAX_STATIC_APS entries.
    let base = AP_BOOTSTRAP_TASKS.get() as usize;
    let stride = core::mem::size_of::<BootstrapTaskAbi>();
    let end = base + stride * MAX_STATIC_APS;
    if p >= base && p < end && (p - base) % stride == 0 {
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
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = AP_BOOTSTRAP_TASKS.ptr_at(i);
    }
    out
}
