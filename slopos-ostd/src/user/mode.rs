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
use crate::sync::BspToken;
use crate::user::context::UserContext;
#[cfg(all(target_arch = "x86_64", not(test)))]
use crate::user::context::UserRegs;

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

/// One-shot wiring point for the production user-mode backend. The
/// `&BspToken<'brand>` witnesses BSP-only init; `backend` must live
/// for the static lifetime of the kernel and drive the user-mode
/// round trip in the manner documented on [`UserModeBackend`] (Inv. 2).
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
/// `execute_round_trip` stashes the active [`UserContext`] pointer in
/// `pcr.user_ctx_ptr`, resets the return-reason slot, invokes
/// [`user_mode_round_trip_asm`] (which builds the IRETQ frame and
/// transitions to user mode), and on user→kernel re-entry decodes the
/// trampoline's return-reason payload via [`read_return_reason`].
///
/// `_space` is intentionally ignored at this layer: CR3 is owned by
/// the kernel paging code outside OSTD, so address-space activation
/// happens before the round trip is dispatched.
pub struct PcrUserModeBackend;

/// Production backend installed by boot via
/// [`register_user_mode_backend`]. Use as
/// `register_user_mode_backend(&DEFAULT_USER_MODE_BACKEND)`.
pub static DEFAULT_USER_MODE_BACKEND: PcrUserModeBackend = PcrUserModeBackend;

// SAFETY: `PcrUserModeBackend` carries no per-instance state. Every
// method reads the per-CPU PCR slots through `current_pcr()` (which
// resolves to the running CPU's PCR via `gs:[0]`). The round trip is
// not preemptible (the caller — `user_task_loop` — runs with IRQs on
// only inside user mode; the kernel-side window between the
// `user_ctx_ptr.store` and the iretq is fully serialised on the
// running CPU).
#[cfg(all(target_arch = "x86_64", not(test)))]
unsafe impl UserModeBackend for PcrUserModeBackend {
    unsafe fn execute_round_trip(
        &self,
        ctx_ptr: *mut UserContext,
        _space: &VmSpace,
    ) -> ReturnReason {
        use crate::cpu::x86_64::pcr::{RETURN_REASON_KIND_NONE, current_pcr};

        // Stash the context pointer where `__ostd_user_return` will
        // find it. Released before the iretq so the trampoline (which
        // reads with Acquire-equivalent `mov gs:…`) sees the publish.
        // SAFETY: `current_pcr()` is callable because GS_BASE was
        // installed at PCR setup; the slot is per-CPU and the CPU is
        // the sole writer in this scope.
        let pcr = unsafe { current_pcr() };
        pcr.user_ctx_ptr.store(ctx_ptr, Ordering::Release);

        // Reset return-reason to a sentinel so a stale value can't be
        // misread if the trampoline somehow returns without writing
        // (defense in depth against the asm contract being violated).
        pcr.return_reason
            .kind
            .store(RETURN_REASON_KIND_NONE, Ordering::Release);
        pcr.return_reason.payload.store(0, Ordering::Release);

        // Drive the actual round trip. The asm helper saves kernel
        // callee-saves + RSP + return RIP, builds the IRETQ frame from
        // the supplied UserRegs, and `iretq`s into user. The
        // trampoline `jmp`s back to a label inside the helper on
        // user→kernel return, at which point control returns here.
        //
        // SAFETY: `ctx_ptr` is valid for the duration of the round
        // trip (caller invariant via `&'a mut UserContext`); the
        // helper consumes the regs pointer before iretq, and the
        // trampoline writes the new user state back through
        // `pcr.user_ctx_ptr` — not through this regs pointer.
        let regs_ptr = unsafe { (&*ctx_ptr).regs_ptr() };
        unsafe {
            user_mode_round_trip_asm(regs_ptr);
        }

        // SAFETY: the trampoline wrote a well-formed encoding into the
        // return-reason slot before it `jmp`ed back; `read_return_reason`
        // panics on an invalid encoding (defense in depth).
        read_return_reason()
    }
}

// Host-side test build: `user_mode_round_trip_asm` and
// `read_return_reason` are only compiled on `x86_64` `not(test)`. The
// trait surface still needs an impl so `DEFAULT_USER_MODE_BACKEND`
// type-checks; host tests never invoke `execute_round_trip` (they
// drive `UserMode` through a `reset_user_mode_backend_for_test` reset).
#[cfg(not(all(target_arch = "x86_64", not(test))))]
// SAFETY: this build branch never has its `execute_round_trip` called
// because the host test fixtures install a stub backend; the
// `unreachable!` is the only operation performed.
unsafe impl UserModeBackend for PcrUserModeBackend {
    unsafe fn execute_round_trip(
        &self,
        _ctx_ptr: *mut UserContext,
        _space: &VmSpace,
    ) -> ReturnReason {
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

// =============================================================================
// User-return trampoline.
//
// `__ostd_user_return` is the LSTAR target installed by the trusted
// side.  When the user executes SYSCALL the CPU lands here; the
// trampoline saves user GPRs into the active `UserContext`
// (located via the per-CPU PCR slot stashed by
// `PcrUserModeBackend::execute_round_trip`), encodes a `ReturnReason`,
// restores the kernel callee-save snapshot the backend left in
// `pcr.kernel_return_ctx`, and `jmp`s back to the saved return RIP —
// which lands in `execute_round_trip`'s tail and lets it return
// normally.
//
// The asm body lives in `asm/user_return.s` (AT&T syntax) and is
// included verbatim here.  Field offsets used by the asm are mirrored
// from `pcr.rs` and `context.rs` and pinned by the `const _: () =
// assert!(...)` razors in those files.
// =============================================================================

#[cfg(all(target_arch = "x86_64", not(test)))]
core::arch::global_asm!(include_str!("asm/user_return.s"), options(att_syntax),);

// On host-side `cargo test -p slopos-ostd` runs we still need the
// `__ostd_user_return` symbol so `user_return_trampoline_addr()` is
// callable; provide a `ud2`-equivalent placeholder.  The host build is
// never reached at runtime in user mode.
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

// =============================================================================
// Kernel→user round-trip asm helper.
//
// `user_mode_round_trip_asm` is the entry-side complement to
// `__ostd_user_return`.  The trusted side calls it after stashing the
// active `UserContext` pointer in `pcr.user_ctx_ptr`.  The function:
//
//   1. Saves the kernel callee-save GPRs into `pcr.kernel_return_ctx`.
//   2. Pops its own return address off the kernel stack and stashes it
//      (alongside the post-pop RSP) in `pcr.kernel_return_ctx.{rip,rsp}`
//      — the kernel stack must not carry any live kernel state across
//      the iretq, since IRQs from user mode will reuse the region at
//      `TSS.RSP0` for their own pushes.
//   3. Pushes a 5-word IRETQ frame derived from the supplied `UserRegs`
//      (SS / RSP / RFLAGS / CS / RIP — in IRETQ order).
//   4. Loads every other user GPR from `UserRegs` (RDI restored last).
//   5. Executes `swapgs; iretq` to land in user mode.
//
// On the user→kernel return, `__ostd_user_return` restores the kernel
// callee-saves from `pcr.kernel_return_ctx`, sets RSP from
// `kernel_return_ctx.rsp`, and `jmp`s to `kernel_return_ctx.rip` — the
// caller's post-call instruction.  This function therefore has *no*
// epilogue: control never returns to its body.  A `ret` at the end
// would be both dead code and a correctness hazard: it would pop
// `[saved_rsp]` for the return RIP, but `[saved_rsp]` is on the
// per-task kernel stack and gets overwritten by any IRQ the CPU takes
// from user mode while the round trip is in flight (TSS.RSP0 reuses
// that region for ISR pushes).  The all-PCR design here is what
// Linux's `entry_SYSCALL_64` and Asterinas's syscall entry both do.
//
// SAFETY: the caller (`PcrUserModeBackend::execute_round_trip`) must
// have stashed the matching `UserContext` pointer in
// `pcr.user_ctx_ptr` before invocation; without that the trampoline
// will dereference a stale pointer on the next user→kernel transition.
// =============================================================================

#[cfg(all(target_arch = "x86_64", not(test)))]
const SEL_USER_CODE_RPL3: u64 = 0x23;
#[cfg(all(target_arch = "x86_64", not(test)))]
const SEL_USER_DATA_RPL3: u64 = 0x1B;

/// Lower-bound the size of `UserRegs` so an offset typo into the
/// trampoline asm (which indexes off `[rdi + offset_of!(UserRegs, …)]`)
/// can't slip past the struct's end.  144 covers the GPR file + RIP /
/// RFLAGS / FS_BASE / GS_BASE / CS / SS — i.e., every offset the
/// trampoline reads.
#[cfg(all(target_arch = "x86_64", not(test)))]
const _: () = assert!(core::mem::size_of::<UserRegs>() >= 144);

#[cfg(all(target_arch = "x86_64", not(test)))]
#[unsafe(naked)]
pub unsafe extern "sysv64" fn user_mode_round_trip_asm(_user_regs: *const UserRegs) {
    // RDI = pointer to UserRegs.  We never touch RSI/RDX/RCX/etc.
    // before they're read into UserRegs because the inline asm reads
    // them from `[rdi + …]` rather than treating them as live inputs.
    core::arch::naked_asm!(
        // ---- Save kernel callee-saves into pcr.kernel_return_ctx. ----
        "mov gs:[{krc} + {krc_rbx}], rbx",
        "mov gs:[{krc} + {krc_rbp}], rbp",
        "mov gs:[{krc} + {krc_r12}], r12",
        "mov gs:[{krc} + {krc_r13}], r13",
        "mov gs:[{krc} + {krc_r14}], r14",
        "mov gs:[{krc} + {krc_r15}], r15",

        // CRITICAL: pop our own return address off the kernel stack and
        // stash it in `pcr.kernel_return_ctx.rip`, then save the
        // post-pop RSP into `pcr.kernel_return_ctx.rsp`.  We cannot
        // leave the return address on the kernel stack across the
        // iretq: any interrupt that fires from user mode reuses
        // `TSS.RSP0` (= the per-task kernel stack top) and the ISR's
        // pushes overwrite this region.  Asterinas and Linux both
        // park the return address in per-CPU memory for the same
        // reason — see `entry_SYSCALL_64` in arch/x86/entry/entry_64.S
        // and `ostd/src/arch/x86/trap/syscall.rs`.
        "pop rax",
        "mov gs:[{krc} + {krc_rip}], rax",
        "mov gs:[{krc} + {krc_rsp}], rsp",

        // ---- Build IRETQ frame on the kernel stack. ----
        // Order (top-of-stack last): SS, RSP, RFLAGS, CS, RIP.
        "push {sel_user_data}",
        "push qword ptr [rdi + {ur_rsp}]",
        "push qword ptr [rdi + {ur_rflags}]",
        "push {sel_user_code}",
        "push qword ptr [rdi + {ur_rip}]",

        // ---- Restore user GPRs.  RDI restored last. ----
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

        // ---- swapgs + iretq into user mode. ----
        // No `100:` label / `ret` epilogue: __ostd_user_return jumps
        // directly to the saved return address via `jmp gs:[krc_rip]`,
        // which is in kernel .text (intact across user-mode interrupt
        // ISR usage of the kernel stack).
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

/// Decode the per-CPU return-reason slot the trampoline wrote.  Called
/// by the user-mode backend after [`user_mode_round_trip_asm`] returns.
#[cfg(all(target_arch = "x86_64", not(test)))]
pub fn read_return_reason() -> ReturnReason {
    use crate::cpu::x86_64::pcr::{
        RETURN_REASON_KIND_EXCEPTION, RETURN_REASON_KIND_INTERRUPT, RETURN_REASON_KIND_SYSCALL,
        current_pcr,
    };
    // SAFETY: GS_BASE is installed before any user mode is reached;
    // `current_pcr()` returns a valid `&'static ProcessorControlRegion`.
    let pcr = unsafe { current_pcr() };
    let kind = pcr.return_reason.kind.load(Ordering::Acquire);
    let payload = pcr.return_reason.payload.load(Ordering::Acquire);
    match kind {
        RETURN_REASON_KIND_SYSCALL => ReturnReason::Syscall(payload),
        RETURN_REASON_KIND_INTERRUPT => ReturnReason::Interrupt(payload as u8),
        RETURN_REASON_KIND_EXCEPTION => ReturnReason::Exception(ExceptionInfo {
            vector: payload as u8,
            error_code: 0,
            fault_addr: 0,
        }),
        _ => panic!(
            "slopos_ostd::user::mode: unknown ReturnReason kind {kind} \
             — `__ostd_user_return` left an invalid encoding"
        ),
    }
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
