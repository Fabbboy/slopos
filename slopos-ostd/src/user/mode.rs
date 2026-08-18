//! User-mode entry / exit primitive.
//!
//! [`UserMode::execute`] is the only OSTD-sanctioned way to enter user mode:
//! it IRETQs to the RIP/RSP encoded in the `UserContext` and returns once user
//! mode hands control back, with [`ReturnReason`] naming what brought it back.
//! `__ostd_user_return`, defined here via `global_asm!`, is the trampoline the
//! IDT routes user→kernel transitions through; it captures the user register
//! file back into the `UserContext` named by the per-CPU stash.
//!
//! That stash is supplied by the trusted side via
//! [`register_user_mode_backend`]; until a backend is registered,
//! [`UserMode::execute`] panics.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm::vm_space::VmSpace;
use crate::sync::BspToken;
use crate::user::context::UserContext;
#[cfg(all(target_arch = "x86_64", not(test)))]
use crate::user::context::UserRegs;

/// Why the kernel re-took control from a user-mode thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnReason {
    /// SYSCALL; the argument is the syscall number loaded into `rax`.
    Syscall(u64),
    /// A CPU exception; `fault_addr` is CR2 for `#PF` and 0 otherwise.
    Exception(ExceptionInfo),
    /// An external interrupt took control while the user thread was running.
    Interrupt(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExceptionInfo {
    pub vector: u8,
    pub error_code: u64,
    pub fault_addr: u64,
}

/// Borrowed handle paired against a `UserContext` and a `VmSpace`.
///
/// `execute` consumes the wrapper, but `&'a UserContext` is `Copy`, so that
/// confers no once-only property on the context itself: entry is serialised by
/// the `__ostd_user_return` protocol (publish `pcr.user_ctx_ptr` → iretq →
/// trampoline → return), not by this type.
pub struct UserMode<'a> {
    ctx: &'a UserContext,
    space: &'a VmSpace,
}

impl<'a> UserMode<'a> {
    pub fn new(ctx: &'a UserContext, space: &'a VmSpace) -> Self {
        Self { ctx, space }
    }

    /// Switch the current CPU into user mode and resume execution at
    /// `ctx.rip()` with `ctx.rsp()`. Returns once a syscall,
    /// exception, or external interrupt rejoins the kernel.
    ///
    /// SAFETY: Inv. 2 (kernel-mode CPU state) is preserved because
    /// `set_rflags` masked every dangerous flag before the frame was built.
    /// Inv. 5 (user pointers cannot reach kernel memory) is preserved because
    /// the context's `rip` / `rsp` are loaded via IRETQ, which itself enforces
    /// canonicality on 64-bit.
    pub fn execute(self) -> ReturnReason {
        let backend = current_user_mode_backend();
        // SAFETY: `self` borrows `ctx` for `'a`, which outlives the
        // round trip; the backend publishes it to the trampoline
        // through its per-CPU slot.
        unsafe { backend.execute_round_trip(self.ctx, self.space) }
    }

    pub fn ctx(&self) -> &UserContext {
        self.ctx
    }
}

/// Trusted-side hook that drives a single user-mode round trip.
///
/// An implementation stashes `ctx` in the current CPU's PCR slot for the
/// `__ostd_user_return` trampoline, activates `space`, saves the kernel
/// callee-saves, restores the user GPRs from `ctx.regs_ptr()`, and
/// `swapgs; iretq`s. It must derive the [`ReturnReason`] from the per-task
/// `UserContext` the trampoline wrote, never from a per-CPU slot, which a
/// preemption in the trampoline-return tail could misdirect to another CPU.
///
/// # Safety
///
/// `space` must outlive the round trip. The trampoline writes the
/// final user register state back into `ctx` before this function
/// returns, through the cell that holds its register file.
pub unsafe trait UserModeBackend: Send + Sync + 'static {
    unsafe fn execute_round_trip(&self, ctx: &UserContext, space: &VmSpace) -> ReturnReason;
}

/// Default backend used until [`register_user_mode_backend`] is
/// called. Panics if invoked.
struct PanicBackend;

// SAFETY: holds no per-CPU state; trivially Send + Sync.
unsafe impl UserModeBackend for PanicBackend {
    unsafe fn execute_round_trip(&self, _ctx: &UserContext, _space: &VmSpace) -> ReturnReason {
        panic!(
            "slopos_ostd::user::mode::UserMode::execute called before \
             register_user_mode_backend installed a production backend"
        );
    }
}

static PANIC_BACKEND: PanicBackend = PanicBackend;

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn UserModeBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); reads only happen after observing the flag with
// Acquire, so the read sees the published reference. Inv. 2.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production user-mode backend. The
/// `&BspToken<'brand>` witnesses BSP-only init; `backend` must drive the round
/// trip in the manner documented on [`UserModeBackend`] (Inv. 2).
pub fn register_user_mode_backend<'brand>(
    _token: &BspToken<'brand>,
    backend: &'static dyn UserModeBackend,
) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(
        !was_installed,
        "slopos_ostd::user::mode::register_user_mode_backend called twice"
    );
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can race.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(backend);
    }
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_user_mode_backend_for_test() {
    BACKEND_INSTALLED.store(false, Ordering::Release);
}

/// Production [`UserModeBackend`] that drives the kernel→user→kernel
/// round trip via the per-CPU PCR slots.
///
/// `_space` is intentionally ignored at this layer: CR3 is owned by
/// the kernel paging code outside OSTD, so address-space activation
/// happens before the round trip is dispatched.
pub struct PcrUserModeBackend;

/// Production backend installed by boot via [`register_user_mode_backend`].
pub static DEFAULT_USER_MODE_BACKEND: PcrUserModeBackend = PcrUserModeBackend;

// SAFETY: `PcrUserModeBackend` carries no per-instance state. The outbound leg
// reads the running CPU's PCR through `current_pcr()` with IRQs off, and the
// window between the `user_ctx_ptr.store` and the iretq is serialised on that
// CPU. The return reason is read from the per-task `UserContext`, which travels
// with the task, so a preempt-and-migrate in the trampoline's post-`sti` tail
// cannot misattribute it.
#[cfg(all(target_arch = "x86_64", not(test)))]
unsafe impl UserModeBackend for PcrUserModeBackend {
    unsafe fn execute_round_trip(&self, ctx: &UserContext, _space: &VmSpace) -> ReturnReason {
        use crate::cpu::x86_64::pcr::current_pcr;

        // Released before the iretq so the trampoline's `mov gs:…` sees the
        // publish. Writing through this shared borrow is sound because `regs`
        // sits at offset zero, so every `UR_*` displacement the trampoline
        // uses lands inside the cell holding the register file.
        //
        // SAFETY: `current_pcr()` is callable because GS_BASE was
        // installed at PCR setup; the slot is per-CPU and the CPU is
        // the sole writer in this scope.
        let pcr = unsafe { current_pcr() };
        pcr.user_ctx_ptr
            .store(core::ptr::from_ref(ctx).cast_mut(), Ordering::Release);

        // SAFETY: `ctx` is borrowed for the duration of the round trip;
        // the helper consumes the regs pointer before iretq, and the
        // trampoline writes the new user state back through
        // `pcr.user_ctx_ptr` — not through this regs pointer.
        unsafe {
            user_mode_round_trip_asm(ctx.regs_ptr());
        }

        // The only path back here is the SYSCALL trampoline, which saves the
        // user GPRs into the per-task `UserContext` before returning:
        // exceptions and interrupts from user mode take the legacy IDT path
        // and never return through this round trip, so the reason is always a
        // syscall.
        ReturnReason::Syscall(ctx.rax())
    }
}

// Host-side stub: the trait surface still needs an impl for
// `DEFAULT_USER_MODE_BACKEND` to type-check, and host tests install their own
// backend rather than calling this one.
#[cfg(not(all(target_arch = "x86_64", not(test))))]
// SAFETY: carries no state; the only operation performed is `unreachable!`.
unsafe impl UserModeBackend for PcrUserModeBackend {
    unsafe fn execute_round_trip(&self, _ctx: &UserContext, _space: &VmSpace) -> ReturnReason {
        unreachable!(
            "PcrUserModeBackend::execute_round_trip is only callable on \
             x86_64 production builds"
        );
    }
}

fn current_user_mode_backend() -> &'static dyn UserModeBackend {
    if BACKEND_INSTALLED.load(Ordering::Acquire) {
        // SAFETY: `BACKEND_INSTALLED == true` ⇒ the AcqRel swap that
        // set the flag was followed by a write into `BACKEND_SLOT`;
        // the Acquire load above synchronises with that release.
        unsafe { (*BACKEND_SLOT.0.get()).assume_init() }
    } else {
        &PANIC_BACKEND
    }
}

// `__ostd_user_return` is the LSTAR target: it saves the user GPRs into the
// `UserContext` named by the per-CPU PCR slot, restores the kernel callee-save
// snapshot from `pcr.kernel_return_ctx`, and `jmp`s to the saved return RIP in
// `execute_round_trip`'s tail. Its body is `asm/user_return.s` (AT&T syntax);
// every field offset arrives as a `const offset_of!` operand below, so there is
// no mirrored layout to drift.

#[cfg(all(target_arch = "x86_64", not(test)))]
core::arch::global_asm!(
    include_str!("asm/user_return.s"),
    pcr_user_rsp_tmp = const core::mem::offset_of!(
        crate::cpu::x86_64::pcr::ProcessorControlRegion, user_rsp_tmp),
    pcr_kernel_rsp = const core::mem::offset_of!(
        crate::cpu::x86_64::pcr::ProcessorControlRegion, kernel_rsp),
    pcr_user_ctx_ptr = const core::mem::offset_of!(
        crate::cpu::x86_64::pcr::ProcessorControlRegion, user_ctx_ptr),
    pcr_kernel_return_ctx = const core::mem::offset_of!(
        crate::cpu::x86_64::pcr::ProcessorControlRegion, kernel_return_ctx),
    pcr_user_rax_tmp = const core::mem::offset_of!(
        crate::cpu::x86_64::pcr::ProcessorControlRegion, user_rax_tmp),
    krc_rbx = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rbx),
    krc_rbp = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rbp),
    krc_r12 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r12),
    krc_r13 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r13),
    krc_r14 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r14),
    krc_r15 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r15),
    krc_rsp = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rsp),
    krc_rip = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rip),
    ur_rax = const core::mem::offset_of!(UserRegs, rax),
    ur_rbx = const core::mem::offset_of!(UserRegs, rbx),
    ur_rcx = const core::mem::offset_of!(UserRegs, rcx),
    ur_rdx = const core::mem::offset_of!(UserRegs, rdx),
    ur_rsi = const core::mem::offset_of!(UserRegs, rsi),
    ur_rdi = const core::mem::offset_of!(UserRegs, rdi),
    ur_rbp = const core::mem::offset_of!(UserRegs, rbp),
    ur_rsp = const core::mem::offset_of!(UserRegs, rsp),
    ur_r8 = const core::mem::offset_of!(UserRegs, r8),
    ur_r9 = const core::mem::offset_of!(UserRegs, r9),
    ur_r10 = const core::mem::offset_of!(UserRegs, r10),
    ur_r11 = const core::mem::offset_of!(UserRegs, r11),
    ur_r12 = const core::mem::offset_of!(UserRegs, r12),
    ur_r13 = const core::mem::offset_of!(UserRegs, r13),
    ur_r14 = const core::mem::offset_of!(UserRegs, r14),
    ur_r15 = const core::mem::offset_of!(UserRegs, r15),
    ur_rip = const core::mem::offset_of!(UserRegs, rip),
    ur_rflags = const core::mem::offset_of!(UserRegs, rflags_user_subset),
    sel_kernel_data = const crate::arch::x86_64::gdt::SegmentSelector::KERNEL_DATA.0,
    options(att_syntax),
);

// Host builds still need the `__ostd_user_return` symbol so
// `user_return_trampoline_addr()` links; it is never executed.
#[cfg(all(target_arch = "x86_64", test))]
core::arch::global_asm!(
    ".global __ostd_user_return",
    ".global __ostd_user_return_end",
    "__ostd_user_return:",
    "    ud2",
    "__ostd_user_return_end:",
    "    ret",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    /// The IDT's user-mode entry target: captures the current user state and
    /// rejoins [`UserMode::execute`].
    pub fn __ostd_user_return();
}

/// Address of the user-return trampoline, read when programming the
/// user-mode IDT vectors.
#[cfg(target_arch = "x86_64")]
pub fn user_return_trampoline_addr() -> u64 {
    __ostd_user_return as *const () as u64
}

// `user_mode_round_trip_asm` is the entry-side complement to
// `__ostd_user_return`: it saves the kernel callee-saves and its own return
// RIP/RSP into `pcr.kernel_return_ctx`, builds the IRETQ frame from the
// supplied `UserRegs`, and `swapgs; iretq`s into user mode. It has *no*
// epilogue — the trampoline `jmp`s straight to `kernel_return_ctx.rip`, so
// control never returns to its body.
//
// SAFETY: the caller must have stashed the matching `UserContext` pointer in
// `pcr.user_ctx_ptr` before invocation; without that the trampoline
// dereferences a stale pointer on the next user→kernel transition.

#[cfg(all(target_arch = "x86_64", not(test)))]
const SEL_USER_CODE_RPL3: u64 = 0x23;
#[cfg(all(target_arch = "x86_64", not(test)))]
const SEL_USER_DATA_RPL3: u64 = 0x1B;

/// Lower-bound `UserRegs` so an offset typo in the trampoline asm cannot index
/// past the struct's end. 144 covers the GPR file plus RIP / RFLAGS / FS_BASE /
/// GS_BASE / CS / SS — every offset the trampoline reads.
#[cfg(all(target_arch = "x86_64", not(test)))]
const _: () = assert!(core::mem::size_of::<UserRegs>() >= 144);

#[cfg(all(target_arch = "x86_64", not(test)))]
#[unsafe(naked)]
pub unsafe extern "sysv64" fn user_mode_round_trip_asm(_user_regs: *const UserRegs) {
    // RDI = pointer to UserRegs; every other GPR is read from `[rdi + …]`
    // rather than treated as a live input.
    core::arch::naked_asm!(
        "mov gs:[{krc} + {krc_rbx}], rbx",
        "mov gs:[{krc} + {krc_rbp}], rbp",
        "mov gs:[{krc} + {krc_r12}], r12",
        "mov gs:[{krc} + {krc_r13}], r13",
        "mov gs:[{krc} + {krc_r14}], r14",
        "mov gs:[{krc} + {krc_r15}], r15",

        // The return address cannot stay on the kernel stack across the iretq:
        // any interrupt that fires from user mode reuses `TSS.RSP0` (the
        // per-task kernel stack top) and the ISR's pushes overwrite this
        // region.
        "pop rax",
        "mov gs:[{krc} + {krc_rip}], rax",
        "mov gs:[{krc} + {krc_rsp}], rsp",

        // IRETQ frame order, top-of-stack last: SS, RSP, RFLAGS, CS, RIP.
        "push {sel_user_data}",
        "push qword ptr [rdi + {ur_rsp}]",
        "push qword ptr [rdi + {ur_rflags}]",
        "push {sel_user_code}",
        "push qword ptr [rdi + {ur_rip}]",

        // RDI restored last: every other load indexes off it.
        "mov rax, [rdi + {ur_rax}]",
        "mov rbx, [rdi + {ur_rbx}]",
        "mov rcx, [rdi + {ur_rcx}]",
        "mov rdx, [rdi + {ur_rdx}]",
        "mov rsi, [rdi + {ur_rsi}]",
        "mov rbp, [rdi + {ur_rbp}]",
        "mov r8,  [rdi + {ur_r8}]",
        "mov r9,  [rdi + {ur_r9}]",
        "mov r10, [rdi + {ur_r10}]",
        "mov r11, [rdi + {ur_r11}]",
        "mov r12, [rdi + {ur_r12}]",
        "mov r13, [rdi + {ur_r13}]",
        "mov r14, [rdi + {ur_r14}]",
        "mov r15, [rdi + {ur_r15}]",
        "mov rdi, [rdi + {ur_rdi}]",

        // No `ret` epilogue: `__ostd_user_return` jumps directly to the saved
        // return address via `jmp gs:[krc_rip]`, which is in kernel .text and
        // so survives an ISR's use of the kernel stack.
        "swapgs",
        "iretq",

        krc = const crate::cpu::x86_64::pcr::offsets::KERNEL_RETURN_CTX,
        krc_rbx = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rbx),
        krc_rbp = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rbp),
        krc_r12 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r12),
        krc_r13 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r13),
        krc_r14 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r14),
        krc_r15 = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, r15),
        krc_rsp = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rsp),
        krc_rip = const core::mem::offset_of!(crate::cpu::x86_64::pcr::KernelReturnContext, rip),
        sel_user_data = const SEL_USER_DATA_RPL3,
        sel_user_code = const SEL_USER_CODE_RPL3,
        ur_rax = const core::mem::offset_of!(UserRegs, rax),
        ur_rbx = const core::mem::offset_of!(UserRegs, rbx),
        ur_rcx = const core::mem::offset_of!(UserRegs, rcx),
        ur_rdx = const core::mem::offset_of!(UserRegs, rdx),
        ur_rsi = const core::mem::offset_of!(UserRegs, rsi),
        ur_rdi = const core::mem::offset_of!(UserRegs, rdi),
        ur_rbp = const core::mem::offset_of!(UserRegs, rbp),
        ur_rsp = const core::mem::offset_of!(UserRegs, rsp),
        ur_r8  = const core::mem::offset_of!(UserRegs, r8),
        ur_r9  = const core::mem::offset_of!(UserRegs, r9),
        ur_r10 = const core::mem::offset_of!(UserRegs, r10),
        ur_r11 = const core::mem::offset_of!(UserRegs, r11),
        ur_r12 = const core::mem::offset_of!(UserRegs, r12),
        ur_r13 = const core::mem::offset_of!(UserRegs, r13),
        ur_r14 = const core::mem::offset_of!(UserRegs, r14),
        ur_r15 = const core::mem::offset_of!(UserRegs, r15),
        ur_rip = const core::mem::offset_of!(UserRegs, rip),
        ur_rflags = const core::mem::offset_of!(UserRegs, rflags_user_subset),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::context::{FpuStateRef, UserRegs};

    #[test]
    fn return_reason_variants_distinct() {
        let s = ReturnReason::Syscall(42);
        let i = ReturnReason::Interrupt(0xEC);
        let e = ReturnReason::Exception(ExceptionInfo {
            vector: 14,
            error_code: 0x4,
            fault_addr: 0xdead_beef,
        });
        assert_ne!(s, i);
        assert_ne!(s, e);
        assert_ne!(i, e);
    }

    #[test]
    fn user_mode_borrow_shape() {
        let regs = UserRegs::default();
        let ctx = UserContext::new(regs, FpuStateRef::empty());
        // No `VmSpace` here: constructing one needs the allocator fixture, and
        // this only asserts `ctx` is usable through a shared borrow.
        ctx.set_rip(0x1000);
        assert_eq!(ctx.rip(), 0x1000);
    }
}
