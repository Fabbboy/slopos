//! SafeStack-sanitizer runtime: the `__safestack_pointer_address`
//! callback LLVM's instrumented prologues call for the data-stack slot
//! address, and the per-CPU [`BootstrapTaskAbi`] stubs that answer it
//! before any real task exists.
//!
//! The slot lives in the `Task`, not the PCR, because LLVM's
//! `safestack-use-pointer-address` mode caches the returned pointer and
//! reuses it across accesses inside a function: a per-CPU slot address
//! would go stale the moment the scheduler migrated the task, and the
//! cached pointer would then name another CPU's PCR. A field inside the
//! task moves with the task.
//!
//! Ordering: `boot/limine_entry.s` (BSP) and the `ap_entry` trampoline
//! (APs) populate their own `PCR.self_ref`, `PCR.current_task` and the
//! referenced stub's `unsafe_stack_sp` before issuing `wrmsr
//! IA32_GS_BASE`. Once the scheduler dispatches the idle task the stub
//! is never used again.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;

/// Maximum number of statically-allocated AP bootstrap stubs.
/// Matches `slopos_arch::pcr::MAX_STATIC_APS`.
pub const MAX_STATIC_APS: usize = 16;

/// Size of each bootstrap data-stack buffer, sized for the early-boot
/// instrumented prologues on the path to `kernel_main` / `ap_entry_rust`.
pub const BOOTSTRAP_UNSAFE_STACK_SIZE: usize = 0x10000;

crate::no_mangle_static! {
    /// Linker-visible copy of [`TASK_UNSAFE_STACK_SP_OFFSET`]: the boot
    /// trampolines stamp each bootstrap Task's `unsafe_stack_sp` in asm,
    /// and a Rust `const` alone is invisible to the linker. `#[used]`
    /// keeps the symbol against dead-code elimination.
    #[used]
    pub static BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64 = TASK_UNSAFE_STACK_SP_OFFSET as u64;
}

#[repr(C, align(16))]
pub struct BootstrapUnsafeStack(pub [u8; BOOTSTRAP_UNSAFE_STACK_SIZE]);

crate::no_mangle_static! {
    pub static BOOTSTRAP_UNSAFE_STACK: SyncUnsafeCell<BootstrapUnsafeStack> =
        SyncUnsafeCell::new(BootstrapUnsafeStack([0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]));
}

#[repr(C, align(16))]
pub struct ApBootstrapUnsafeStacks(pub [[u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS]);

/// AP data stacks. Reached by address through `ap_unsafe_stack_top`, never
/// by symbol name — the AP trampoline is OSTD's Rust naked function, not asm.
pub static APS_BOOTSTRAP_UNSAFE_STACKS: SyncUnsafeCell<ApBootstrapUnsafeStacks> =
    SyncUnsafeCell::new(ApBootstrapUnsafeStacks(
        [[0u8; BOOTSTRAP_UNSAFE_STACK_SIZE]; MAX_STATIC_APS],
    ));

/// The prefix of `Task` that pre-heap bootstrap needs, and nothing else.
///
/// The SafeStack prologue asks `gs:[CURRENT_TASK]` then
/// `[rax + TASK_UNSAFE_STACK_SP_OFFSET]`, and that offset is zero, so eight
/// bytes suffice. Being a distinct type is the point: nothing can spell a stub
/// as `*mut Task`, so the "PCR names a stub" case cannot reach a task accessor
/// and read past the object. Both PCR readers filter stubs out —
/// `CurrentTask::get` by the `INVALID_TASK_ID` the stub is always published
/// with, `IdleTask::current` because the idle slot never holds one.
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

crate::no_mangle_static! {
    pub static BSP_BOOTSTRAP_TASK: BootstrapTaskCell = BootstrapTaskCell::new();
}

/// AP bootstrap stubs. Same as the AP stacks: reached by address, not name.
pub static AP_BOOTSTRAP_TASKS: BootstrapTaskArrayCell = BootstrapTaskArrayCell::new();

/// Top of the BSP bootstrap data stack. The end-of-buffer pointer is
/// one-past-the-end and never dereferenced, so `wrapping_add` suffices.
#[inline]
pub fn bsp_bootstrap_unsafe_sp() -> *mut u8 {
    let base = BOOTSTRAP_UNSAFE_STACK.get() as *mut u8;
    let top = base.wrapping_add(BOOTSTRAP_UNSAFE_STACK_SIZE);
    // Align down to 16 bytes for x86-64 System V stack alignment.
    ((top as usize) & !0xF) as *mut u8
}

/// Top of AP slot `i`'s bootstrap data stack (0-based: slot `i` is the
/// AP whose `cpu_info.extra` was set to `i + 1`).
#[inline]
pub fn ap_bootstrap_unsafe_sp(i: usize) -> *mut u8 {
    debug_assert!(i < MAX_STATIC_APS);
    let base = APS_BOOTSTRAP_UNSAFE_STACKS.get() as *mut u8;
    let top_off = (i + 1) * BOOTSTRAP_UNSAFE_STACK_SIZE;
    let top = base.wrapping_add(top_off);
    ((top as usize) & !0xF) as *mut u8
}

/// Seed every bootstrap Task stub with a valid `unsafe_stack_sp`.
///
/// Single-writer: must run exactly once on the BSP before any AP
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

/// Return `true` if `ptr` is one of the statically-allocated bootstrap
/// Task stubs. The scheduler's pre-first-dispatch window legitimately
/// observes a stub as `PCR.current_task`, so raw readers need a way to
/// tell one from a registry-owned task by address.
pub fn is_bootstrap_task_ptr(ptr: *const ()) -> bool {
    if ptr.is_null() {
        return false;
    }
    let p = ptr as usize;
    if p == BSP_BOOTSTRAP_TASK.get() as usize {
        return true;
    }
    let base = AP_BOOTSTRAP_TASKS.get() as usize;
    let stride = core::mem::size_of::<BootstrapTaskAbi>();
    let end = base + stride * MAX_STATIC_APS;
    if p >= base && p < end && (p - base) % stride == 0 {
        return true;
    }
    false
}

/// Raw pointers to every AP bootstrap task stub; entry `i` is the stub
/// for AP slot `i + 1`.
///
/// Caller must not race with [`init_bootstrap_tasks`]; the returned
/// pointers are valid for the lifetime of the kernel image.
pub fn ap_bootstrap_task_ptrs() -> [*mut (); MAX_STATIC_APS] {
    let mut out = [core::ptr::null_mut::<()>(); MAX_STATIC_APS];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = AP_BOOTSTRAP_TASKS.ptr_at(i);
    }
    out
}
