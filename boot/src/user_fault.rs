use core::ffi::CStr;

use slopos_abi::task::TaskFaultReason;
use slopos_arch::InterruptFrame;
use slopos_arch::cpu;
use slopos_kernel_services::kernel_vm_space::activate_post_user_fault;
use slopos_ostd::{kdiag_dump_interrupt_frame, klog_info};
use slopos_sched::scheduler::schedule;
use slopos_sched::task::TaskRef;
use slopos_sched::task::task_find_by_cr3;
use slopos_sched::task::task_terminate;

use crate::panic::set_panic_cpu_state;

/// Retire this CPU after a fatal user-mode exception.
///
/// Terminates the faulting task, attempts to schedule the next runnable
/// task, and — if no context switch occurred (e.g. the scheduler is
/// already disabled during shutdown) — switches to the kernel address
/// space, re-enables interrupts so the CPU can still service IPIs (TLB
/// shootdown, halt broadcast), and parks in a halt loop.
///
/// # Safety invariant
/// The caller must be handling a user-mode exception (CS RPL == 3).
/// This function never returns.
fn retire_faulted_cpu(task_ref: TaskRef, reason: TaskFaultReason) -> ! {
    let tid = task_ref.record_user_fault_exit(reason);
    task_terminate(tid);
    // Release the registry upgrade before the diverging switch tail.
    drop(task_ref);
    // This is the one exception path that abandons the per-CPU exception
    // SafeStack data stack WITHOUT unwinding: `schedule()` switches away
    // and never returns here, so the depth consumed by this handler
    // chain would leak and accumulate across successive fatal user
    // faults (a normal exception restores the slot to its *entry* value,
    // not to the top, so it never heals the leak). Re-prime the slot to
    // the data-stack top with a NAKED store — an instrumented setter,
    // running here on the IST stack, would resolve THIS slot as its own
    // data-SP and undo the write on return — and call it directly so no
    // intervening epilogue clobbers it. Any residual leak (e.g. the
    // post-reprime `schedule()` frame on a switch-away) is bounded and
    // caught as a clean overflow panic by `exc_dstack_guard_fault`.
    let dstack_top = crate::ist_stacks::exc_dstack_top_current_cpu();
    slopos_arch::pcr::reset_ist_unsafe_sp(dstack_top);
    // This handler diverges into `schedule()` and never returns, so the
    // exception-entry preempt hold taken in the common IST dispatcher
    // (its RAII guard, one `irq_entry_bump`) would otherwise leak —
    // leaving the per-CPU preempt count stuck at >=1 for whatever task
    // is scheduled next, which both suppresses its preemption and trips
    // the `schedule_internal` "switch with preempt_count != 0" guard.
    // Release that single hold explicitly so we deschedule at the
    // running baseline and the count saved into this (Zombie) task is 0.
    slopos_ostd::cpu::preempt::release_diverging_exception_hold();
    schedule();
    // schedule() returned without switching — park safely on the
    // kernel master PML4 so this CPU can keep servicing IPIs.
    let _ = activate_post_user_fault();
    cpu::enable_interrupts();
    cpu::halt_loop();
}

pub(crate) fn in_user(frame: &InterruptFrame) -> bool {
    (frame.cs & 0x3) == 0x3
}

pub(crate) fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_until_nul(bytes).expect("cstr_from_bytes input must be NUL-terminated")
}

#[inline]
pub(crate) fn resolve_user_fault_task() -> Option<TaskRef> {
    let hw_cr3 = cpu::read_cr3() & !0xFFF;

    // Recoverable user page fault, not a panic path: the registry lookups stay.
    // Resolving the id to a `TaskRef` first makes the guard itself the validity
    // proof — an address test only asks whether the registry knows that
    // address, never whether it still will — and hands back a borrow, so the
    // CR3 read comes off the guard rather than a raw projection. A bootstrap
    // stub has no valid id and so falls through to the CR3 scan, as before.
    if let Some(task_ref) = slopos_sched::task_struct::Current::get()
        .map(|current| current.id())
        .and_then(slopos_sched::task::task_find_by_id)
    {
        let task_cr3 = task_ref.context_cr3() & !0xFFF;
        if task_cr3 == hw_cr3 {
            return Some(task_ref);
        }
    }

    task_find_by_cr3(hw_cr3)
}

pub(crate) fn terminate_user_task(
    reason: TaskFaultReason,
    frame: &InterruptFrame,
    detail: &'static CStr,
) {
    let Some(task_ref) = resolve_user_fault_task() else {
        klog_info!(
            "Terminating user fault context without a valid current task: {}",
            detail.to_str().unwrap_or("<invalid utf-8>")
        );
        kdiag_dump_interrupt_frame(frame as *const _);
        panic_with_frame(
            "user fault with invalid current task",
            frame as *const _ as *mut _,
        );
        return;
    };
    let tid = task_ref.task_id;
    let detail_str = detail.to_str().unwrap_or("<invalid utf-8>");
    let cr2 = cpu::read_cr2();
    let (rip, rsp, vec, err) = (frame.rip, frame.rsp, frame.vector, frame.error_code);
    let name_raw = task_ref.name_bytes();
    let name_len = name_raw
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_raw.len());
    let name_str = core::str::from_utf8(&name_raw[..name_len]).unwrap_or("<invalid utf-8>");
    let entry_point = task_ref.entry_point;
    let proc_id = task_ref.process_id;
    let flags = task_ref.flags;
    klog_info!(
        "Terminating user task {} ('{}'): {} | vec={} err=0x{:x} cr2=0x{:x} rip=0x{:x} rsp=0x{:x} entry=0x{:x} pid={} flags=0x{:x}",
        tid,
        name_str,
        detail_str,
        vec,
        err,
        cr2,
        rip,
        rsp,
        entry_point,
        proc_id,
        flags
    );
    kdiag_dump_interrupt_frame(frame as *const _);
    retire_faulted_cpu(task_ref, reason);
}

pub(crate) fn panic_with_frame(message: &str, frame: *mut InterruptFrame) {
    if let Some(frame_ref) = InterruptFrame::from_ptr(frame) {
        set_panic_cpu_state(frame_ref.rip, frame_ref.rsp, frame_ref.rbp);
    }
    panic!("{}", message);
}
