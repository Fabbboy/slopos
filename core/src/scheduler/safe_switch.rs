//! Safe context switch implementation using Rust inline assembly.
//!
//! This module provides a high-level context switch that handles:
//! - FPU state save/restore (via inline assembly)
//! - Page table switching (CR3)
//! - The actual register switch (via switch_asm)
//! - SMP synchronization

use core::sync::atomic::{AtomicBool, Ordering, fence};

use slopos_abi::task::{FpuState, SwitchContext, TASK_FLAG_FPU_INITIALIZED, Task};

use super::switch_asm::switch_registers;

static CONTEXT_SWITCH_LOCK: AtomicBool = AtomicBool::new(false);

fn acquire_switch_lock() {
    while CONTEXT_SWITCH_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn release_switch_lock() {
    CONTEXT_SWITCH_LOCK.store(false, Ordering::Release);
}

#[inline(always)]
unsafe fn save_fpu_state(state: &mut FpuState) {
    unsafe {
        core::arch::asm!(
            "fxsave64 [{}]",
            in(reg) state.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

#[inline(always)]
unsafe fn restore_fpu_state(state: &FpuState) {
    unsafe {
        core::arch::asm!(
            "fxrstor64 [{}]",
            in(reg) state.as_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

#[inline(always)]
unsafe fn read_cr3() -> u64 {
    let cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    cr3
}

#[inline(always)]
unsafe fn write_cr3(val: u64) {
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Perform a safe context switch between two tasks.
///
/// This function:
/// 1. Acquires the global context switch lock (SMP safety)
/// 2. Issues memory barrier
/// 3. Saves FPU state for prev task (if initialized)
/// 4. Switches CR3 if needed
/// 5. Restores FPU state for next task (if initialized)
/// 6. Performs the actual register switch
/// 7. Releases the lock and issues memory barrier
///
/// # Safety
///
/// - prev_task may be null (first switch from boot)
/// - next_task must be valid and properly initialized
/// - Must be called with interrupts disabled
pub unsafe fn safe_context_switch(prev_task: *mut Task, next_task: *mut Task) {
    if next_task.is_null() {
        return;
    }

    acquire_switch_lock();
    fence(Ordering::SeqCst);

    let prev_switch_ctx = if !prev_task.is_null() {
        // SAFETY: prev_task is non-null, caller guarantees validity
        unsafe {
            if (*prev_task).flags & TASK_FLAG_FPU_INITIALIZED != 0 {
                save_fpu_state(&mut (*prev_task).fpu_state);
            }
            &raw mut (*prev_task).switch_ctx
        }
    } else {
        core::ptr::null_mut()
    };

    // SAFETY: next_task is non-null (checked above), caller guarantees validity
    let next_cr3 = unsafe { (*next_task).context.cr3 };
    let current_cr3 = unsafe { read_cr3() };

    if next_cr3 != 0 && next_cr3 != current_cr3 {
        // SAFETY: valid CR3 value from task's page directory
        unsafe { write_cr3(next_cr3) };
    }

    // SAFETY: next_task is valid
    unsafe {
        if (*next_task).flags & TASK_FLAG_FPU_INITIALIZED != 0 {
            restore_fpu_state(&(*next_task).fpu_state);
        }
    }

    // SAFETY: both contexts are valid (or prev is null which switch_registers handles)
    unsafe {
        switch_registers(prev_switch_ctx, &(*next_task).switch_ctx);
    }

    fence(Ordering::SeqCst);
    release_switch_lock();
}

/// Initialize a task's switch context for first execution.
///
/// Sets up the switch context so when switch_registers loads it,
/// the task will begin executing at entry_point with arg as the first parameter.
pub fn init_task_switch_context(task: &mut Task, stack_top: u64, entry_point: u64, arg: u64) {
    use super::switch_asm::task_entry_trampoline;

    task.switch_ctx = SwitchContext::zero();
    task.switch_ctx.r12 = entry_point;
    task.switch_ctx.r13 = arg;
    task.switch_ctx.rflags = 0x202;

    let trampoline_addr = task_entry_trampoline as *const () as u64;

    unsafe {
        let return_addr_slot = (stack_top - 8) as *mut u64;
        core::ptr::write_volatile(return_addr_slot, trampoline_addr);
    }

    task.switch_ctx.rsp = stack_top - 8;
    task.switch_ctx.rip = trampoline_addr;
}

/// Save current CPU context to a SwitchContext.
///
/// Used to save the kernel's context before starting the scheduler,
/// so we can return to kernel main when the scheduler stops.
pub unsafe fn save_current_context(ctx: &mut SwitchContext) {
    super::switch_asm::init_current_context(ctx as *mut SwitchContext);
}
