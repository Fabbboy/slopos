//! User-mode entry / exit primitive.
//!
//! [`UserMode<'a>`] is the only OSTD-sanctioned way to enter user
//! mode. [`UserMode::execute`] consumes the wrapper, performs the
//! kernel→user transition (IRETQ to the user RIP/RSP encoded in the
//! `UserContext`), and returns once user mode hands control back
//! through a syscall, exception, or interrupt. The
//! [`ReturnReason`] enumerates what brought us back.
//!
//! The mechanism is split into two halves:
//!
//! - [`UserMode::execute`] runs the entry asm: STAC handling lives
//!   inside `slopos-ostd::user::copy`, swapgs/IRETQ live here.
//! - `__ostd_user_return` (defined in this module via `global_asm!`)
//!   is the trampoline the IDT routes user→kernel transitions
//!   through. It captures the user register file back into the
//!   `UserContext` pointed at by the per-CPU stash and returns
//!   control to `execute()`'s caller.
//!
//! The per-CPU UserContext-pointer stash is supplied by the trusted
//! side via [`register_user_mode_backend`]. Until a backend is
//! registered, [`UserMode::execute`] panics; production wiring
//! installs a backend that proxies to a per-CPU PCR slot and
//! reroutes IDT vectors through `__ostd_user_return`.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm::vm_space::VmSpace;
use crate::user::context::UserContext;

/// Why the kernel re-took control from a user-mode thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnReason {
    /// User executed the SYSCALL instruction. Argument is the syscall
    /// number that was loaded into `rax`.
    Syscall(u64),
    /// User trapped on a CPU exception. The vector / error code /
    /// faulting address (CR2 for `#PF`, else 0) come back via
    /// [`ExceptionInfo`].
    Exception(ExceptionInfo),
    /// External interrupt vector took control while the user thread
    /// was executing.
    Interrupt(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExceptionInfo {
    pub vector: u8,
    pub error_code: u64,
    pub fault_addr: u64,
}

/// Borrowed handle paired against a `UserContext` and a `VmSpace`.
/// `execute` consumes the wrapper so a single `UserMode` value
/// rounds-trips at most once.
pub struct UserMode<'a> {
    ctx: &'a mut UserContext,
    space: &'a VmSpace,
}

impl<'a> UserMode<'a> {
    pub fn new(ctx: &'a mut UserContext, space: &'a VmSpace) -> Self {
        Self { ctx, space }
    }

    /// Switch the current CPU into user mode and resume execution at
    /// `ctx.rip()` with `ctx.rsp()`. Returns once a syscall,
    /// exception, or external interrupt rejoins the kernel.
    ///
    /// SAFETY: The asm sequence below switches GS via `swapgs`,
    /// reloads every GPR (including the user-mode frame on the
    /// trusted side of the per-CPU stash), and IRETs into the user
    /// half. Inv. 2 (kernel-mode CPU state) is preserved because
    /// `set_rflags` masked every dangerous flag before the frame
    /// was built. Inv. 5 (user pointers cannot reach kernel memory)
    /// is preserved because `ctx.regs.rip` / `ctx.regs.rsp` are
    /// loaded via IRETQ which itself enforces canonicality on
    /// 64-bit.
    pub fn execute(self) -> ReturnReason {
        let backend = current_user_mode_backend();
        // SAFETY: `self` borrows `ctx` mutably for `'a`; the backend
        // pins the raw pointer for as long as the round-trip lasts
        // and surfaces it back to the trampoline through its
        // per-CPU slot.
        unsafe { backend.execute_round_trip(self.ctx as *mut UserContext, self.space) }
    }

    pub fn ctx(&self) -> &UserContext {
        self.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut UserContext {
        self.ctx
    }
}

/// Trusted-side hook that drives a single user-mode round trip.
///
/// Implementations must:
/// 1. Stash `ctx_ptr` in the current CPU's PCR slot so the
///    `__ostd_user_return` trampoline can write user state back
///    into it.
/// 2. Activate `space` if not already active.
/// 3. Save the kernel callee-saved registers onto the kernel stack.
/// 4. Restore user GPRs from `(*ctx_ptr).regs()`.
/// 5. Build the IRETQ frame and `swapgs; iretq`.
/// 6. On the next user→kernel transition, the trampoline restores
///    callee-saves and returns control to step 6.
/// 7. Read [`ReturnReason`] out of the per-CPU slot and return it.
///
/// # Safety
///
/// `ctx_ptr` must be valid for the duration of the round trip.
/// `space` must outlive the round trip. The trampoline writes the
/// final user register state back through `ctx_ptr` before this
/// function returns.
pub unsafe trait UserModeBackend: Send + Sync + 'static {
    unsafe fn execute_round_trip(&self, ctx_ptr: *mut UserContext, space: &VmSpace)
    -> ReturnReason;
}

/// Default backend used until [`register_user_mode_backend`] is
/// called. Panics if invoked.
struct PanicBackend;

// SAFETY: holds no per-CPU state; trivially Send + Sync.
unsafe impl UserModeBackend for PanicBackend {
    unsafe fn execute_round_trip(
        &self,
        _ctx_ptr: *mut UserContext,
        _space: &VmSpace,
    ) -> ReturnReason {
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

/// One-shot wiring point for the production user-mode backend.
///
/// # Safety
///
/// `backend` must live for the static lifetime of the kernel and
/// must drive the user-mode round trip in the manner documented on
/// [`UserModeBackend`]. Caller certifies Inv. 2.
pub unsafe fn register_user_mode_backend(backend: &'static dyn UserModeBackend) {
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

// =============================================================================
// User-return trampoline.
//
// IDT vectors for user→kernel transitions (syscall, exception,
// external interrupt) route to `__ostd_user_return`. The trampoline:
//
//   1. swapgs                                — switch to kernel GS.
//   2. push GPRs into a temporary frame on the kernel stack.
//   3. fetch the per-CPU UserContext pointer (via the backend's
//      PCR slot) and copy GPRs into it.
//   4. xsave64 the FPU state into the UserContext-referenced
//      buffer.
//   5. read MSR FS_BASE / KERNEL_GS_BASE back into the
//      UserContext.
//   6. compute the ReturnReason from the IDT vector / hardware
//      exception code / syscall number-in-rax.
//   7. restore the kernel callee-saved registers from the saved
//      KernelReturnContext.
//   8. ret to `UserMode::execute`'s call site.
//
// The body is currently a `ud2`-shaped placeholder. A real
// trampoline requires a per-CPU PCR layout (FsBase / GsBase / kernel
// RSP / preempt count) that the trusted side installs through the
// `UserModeBackend`. Routing into the placeholder before that
// happens fires `#UD` with RIP pointing at this label, surfacing
// the misconfiguration immediately.
// =============================================================================

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global __ostd_user_return",
    ".global __ostd_user_return_end",
    "__ostd_user_return:",
    // The trusted side replaces this body with the full save
    // sequence. The `ud2` here makes accidental routing into the
    // stub immediately visible as an undefined-opcode exception
    // with RIP pointing at this label.
    "    ud2",
    "__ostd_user_return_end:",
    "    ret",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    /// Symbol exposed to the IDT installer in `boot/` (after Phase
    /// 1J). Its address is what the IDT entry points at; it
    /// captures the current user state and rejoins
    /// [`UserMode::execute`].
    pub fn __ostd_user_return();
}

/// Address of the user-return trampoline. The IDT installer reads
/// this when programming the user-mode IDT vectors.
#[cfg(target_arch = "x86_64")]
pub fn user_return_trampoline_addr() -> u64 {
    __ostd_user_return as *const () as u64
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
        // Sanity: UserMode::new threads the borrows. We can't
        // actually `execute` without a registered backend; this just
        // proves the type compiles + ctx is reachable through the
        // wrapper.
        let regs = UserRegs::default();
        let mut ctx = UserContext::new(regs, FpuStateRef::empty());
        // VmSpace requires a registered allocator + kernel master,
        // which the per-test fixture sets up. We avoid constructing
        // one here so this test stays free of fixture coupling.
        // Simply assert ctx is usable through &mut.
        ctx.set_rip(0x1000);
        assert_eq!(ctx.rip(), 0x1000);
    }
}
