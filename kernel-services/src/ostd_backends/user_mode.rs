//! `UserModeBackend` impl that drives the kernel→user→kernel round
//! trip via the per-CPU PCR slots.
//!
//! The kernel-side entry point is [`PcrUserModeBackend::execute_round_trip`]:
//!
//! 1. Stash the active `UserContext` pointer in `pcr.user_ctx_ptr`.
//! 2. Reset the per-CPU return-reason slot to a sentinel.
//! 3. Call `slopos_ostd::user::mode::user_mode_round_trip_asm`, which
//!    saves the kernel callee-save snapshot into `pcr.kernel_return_ctx`
//!    and IRETQs into user mode.
//! 4. When the trampoline (`__ostd_user_return`) reasserts kernel
//!    control on a syscall / exception / interrupt, it writes the
//!    captured user state back through `pcr.user_ctx_ptr`, encodes a
//!    `ReturnReason` into `pcr.return_reason`, restores the kernel
//!    callee-save snapshot, and `jmp`s back into the round-trip helper
//!    — which `ret`s to its caller.
//! 5. We decode the return-reason slot via
//!    `slopos_ostd::user::mode::read_return_reason`.
//!
//! `_space` is intentionally ignored: CR3 is still managed by the
//! legacy paging code outside OSTD; the address space the user task
//! runs in is whichever PML4 the scheduler last loaded.  `VmSpace`-
//! based activation will be wired in once the per-process address-
//! space migration into OSTD lands.

use core::sync::atomic::Ordering;

use slopos_ostd::cpu::x86_64::pcr::{current_pcr, RETURN_REASON_KIND_NONE};
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::user::mode::{ReturnReason, UserModeBackend};

pub struct PcrUserModeBackend;

pub static PCR_USER_MODE: PcrUserModeBackend = PcrUserModeBackend;

// SAFETY: `PcrUserModeBackend` carries no per-instance state.  Every
// method reads the per-CPU PCR slots through `current_pcr()` (which
// resolves to the running CPU's PCR via `gs:[0]`).  The round-trip is
// not preemptible (the caller — `user_task_loop` — runs with IRQs on
// only inside user mode; the kernel-side window between
// `user_ctx_ptr.store` and `iretq` is fully serialised on the running
// CPU).
unsafe impl UserModeBackend for PcrUserModeBackend {
    unsafe fn execute_round_trip(
        &self,
        ctx_ptr: *mut UserContext,
        _space: &VmSpace,
    ) -> ReturnReason {
        // Stash the context pointer where `__ostd_user_return` will
        // find it.  Released before the iretq so the trampoline (which
        // reads with Acquire-equivalent `mov gs:…`) sees the publish.
        // SAFETY: `current_pcr()` is callable because GS_BASE was
        // installed at PCR setup; the slot is per-CPU and the CPU
        // is the sole writer in this scope.
        let pcr = unsafe { current_pcr() };
        pcr.user_ctx_ptr.store(ctx_ptr, Ordering::Release);

        // Reset return-reason to a sentinel so a stale value can't be
        // misread if the trampoline somehow returns without writing
        // (defense in depth against the asm contract being violated).
        pcr.return_reason
            .kind
            .store(RETURN_REASON_KIND_NONE, Ordering::Release);
        pcr.return_reason.payload.store(0, Ordering::Release);

        // Drive the actual round trip.  The asm helper saves kernel
        // callee-saves + RSP + return RIP, builds the IRETQ frame from
        // the supplied UserRegs, and `iretq`s into user.  The
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
            slopos_ostd::user::mode::user_mode_round_trip_asm(regs_ptr);
        }

        // SAFETY: the trampoline wrote a well-formed encoding into the
        // return-reason slot before it `jmp`ed back; `read_return_reason`
        // panics on an invalid encoding (defense in depth).
        slopos_ostd::user::mode::read_return_reason()
    }
}
