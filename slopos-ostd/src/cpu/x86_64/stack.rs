//! Stack and frame pointer register reads.

use core::arch::asm;

#[inline(always)]
pub fn read_rbp() -> u64 {
    let rbp: u64;
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    rbp
}

#[inline(always)]
pub fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

#[inline(always)]
pub fn read_r15() -> u64 {
    let r15: u64;
    unsafe {
        asm!("mov {}, r15", out(reg) r15, options(nomem, nostack, preserves_flags));
    }
    r15
}

/// One-shot stack switch: set `rsp` to `stack_top`, sync `rbp`, then
/// `call target(arg0, arg1)`.
///
/// `arg0` lands in `rdi` and `arg1` lands in `rsi` per the SysV-AMD64
/// ABI, matching `target`'s `extern "C"` signature.
///
/// # Safety
///
/// - `stack_top` must point at the top of a kernel-owned stack
///   reservation with at least `target`'s frame budget available.
/// - The address must be 16-byte aligned so the callee sees
///   `(rsp + 8) % 16 == 0` after the `call` pushes the return address
///   (the SysV-AMD64 prologue alignment contract).
/// - `target` must not return; if it does, `ud2` is executed.
/// - Caller is responsible for ensuring no live references into the
///   previous stack remain when this fn is invoked (a stack switch
///   invalidates every borrow rooted in the old frame).
#[inline(never)]
pub unsafe fn switch_stack_and_call_noreturn(
    stack_top: u64,
    target: extern "C" fn(usize, *mut ()) -> !,
    arg0: usize,
    arg1: *mut (),
) -> ! {
    unsafe {
        asm!(
            "mov rsp, {stack_top}",
            "mov rbp, rsp",
            "call {target}",
            "ud2",
            stack_top = in(reg) stack_top,
            target = in(reg) target,
            in("rdi") arg0,
            in("rsi") arg1,
            options(noreturn),
        );
    }
}

/// Safe wrapper: the scheduler-bringup path discharges
/// [`switch_stack_and_call_noreturn`]'s contract — the stack top comes from the
/// kstack allocator and is 16-byte aligned by construction, `target` diverges,
/// and every consumer above this frame has already returned, so no reference
/// into the previous stack is live.
#[inline]
pub fn enter_scheduler_loop_noreturn(
    stack_top: u64,
    target: extern "C" fn(usize, *mut ()) -> !,
    cpu_id: usize,
    idle_task: *mut (),
) -> ! {
    // SAFETY: see fn-level docs; the scheduler bringup path upholds
    // every clause of `switch_stack_and_call_noreturn`'s contract.
    unsafe { switch_stack_and_call_noreturn(stack_top, target, cpu_id, idle_task) }
}
