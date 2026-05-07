use core::ffi::c_char;

use slopos_abi::addr::PhysAddr;
use slopos_abi::task::TaskFaultReason;
use slopos_arch::InterruptFrame;
use slopos_arch::cpu;
use slopos_core::scheduler::task::{
    task_context_cr3, task_context_rip, task_context_rsp, task_flags, task_id_of,
    task_kernel_stack_bounds, task_name_bytes, task_process_id,
};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::process_vm;
use slopos_utils::string::cstr_to_str_lossy;
use slopos_utils::{kdiag_dump_interrupt_frame, kdiag_stack_word_at, klog_info};

use crate::ist_stacks;
use crate::user_fault::*;

pub(crate) fn exception_default_panic(frame: *mut InterruptFrame) {
    klog_info!("FATAL: Unhandled exception");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame("Unhandled exception", frame);
}

pub(crate) fn exception_fatal(frame: *mut InterruptFrame) {
    let name = frame_exception_name(frame);
    klog_info!("FATAL: {}", name);
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(name, frame);
}

pub(crate) fn exception_nonfatal(frame: *mut InterruptFrame) {
    let name = frame_exception_name(frame);
    klog_info!("ERROR: {}", name);
    kdiag_dump_interrupt_frame(frame);
}

pub(crate) fn frame_exception_name(frame: *mut InterruptFrame) -> &'static str {
    let vector = match InterruptFrame::from_ptr(frame) {
        Some(f) => (f.vector & 0xFF) as u8,
        None => return "<null frame>",
    };
    slopos_arch::arch::exception::get_exception_name(vector)
}

pub(crate) fn exception_invalid_opcode(frame: *mut InterruptFrame) {
    if let Some(f) = InterruptFrame::from_ptr(frame) {
        if in_user(f) {
            terminate_user_task(
                TaskFaultReason::UserUd,
                f,
                cstr_from_bytes(b"invalid opcode in user mode\0"),
            );
            return;
        }
    }
    exception_fatal(frame);
}

pub(crate) fn exception_device_not_available(frame: *mut InterruptFrame) {
    if let Some(f) = InterruptFrame::from_ptr(frame) {
        if in_user(f) {
            terminate_user_task(
                TaskFaultReason::UserDeviceNa,
                f,
                cstr_from_bytes(b"device not available in user mode\0"),
            );
            return;
        }
    }
    exception_nonfatal(frame);
}

pub(crate) fn exception_general_protection(frame: *mut InterruptFrame) {
    if let Some(f) = InterruptFrame::from_ptr(frame) {
        if in_user(f) {
            terminate_user_task(
                TaskFaultReason::UserGp,
                f,
                cstr_from_bytes(b"general protection from user mode\0"),
            );
            return;
        }
    }
    exception_fatal(frame);
}

pub(crate) fn exception_page_fault(frame: *mut InterruptFrame) {
    let fault_addr = cpu::read_cr2();
    let Some(frame_ref) = InterruptFrame::from_ptr(frame) else {
        klog_info!("FATAL: page fault with null frame pointer");
        panic!("page fault with null frame");
    };

    let mut stack_name: *const c_char = core::ptr::null();
    if ist_stacks::ist_guard_fault(fault_addr, &mut stack_name) != 0 {
        klog_info!("FATAL: IST stack overflow detected via guard page");
        if !stack_name.is_null() {
            klog_info!("Stack: {}", cstr_to_str_lossy(stack_name));
        }
        klog_info!("Fault address: 0x{:x}", fault_addr);
        kdiag_dump_interrupt_frame(frame);
        panic_with_frame("IST stack overflow", frame);
        return;
    }

    let from_user = in_user(frame_ref);

    klog_info!("FATAL: Page fault");
    klog_info!("Fault address: 0x{:x}", fault_addr);
    let present = if (frame_ref.error_code & 1) != 0 {
        "Page present"
    } else {
        "Page not present"
    };
    let access = if (frame_ref.error_code & 2) != 0 {
        "Write"
    } else {
        "Read"
    };
    let privilege = if (frame_ref.error_code & 4) != 0 {
        "User"
    } else {
        "Supervisor"
    };
    klog_info!(
        "Error code: 0x{:x} ({}) ({}) ({})",
        frame_ref.error_code,
        present,
        access,
        privilege
    );

    if from_user {
        log_user_page_fault_diagnostics(frame_ref, fault_addr);
        terminate_user_task(
            TaskFaultReason::UserPage,
            frame_ref,
            cstr_from_bytes(b"user page fault\0"),
        );
        return;
    }

    // Supervisor instruction-fetch fault — dump stack words around RSP
    // to help diagnose which return address was corrupted.
    if (frame_ref.error_code & 0x10) != 0 {
        klog_info!("=== STACK DUMP at RSP 0x{:x} ===", frame_ref.rsp);

        // Determine a safe probe range for the stack dump.  Call
        // scheduler_get_current_task() once and reuse the pointer for
        // both bounds computation and the task-context dump below.
        let task_ptr = slopos_core::sched::scheduler_get_current_task();
        let (stack_lo, stack_hi) = match task_kernel_stack_bounds(task_ptr) {
            Some((base, top)) if base != 0 && top > base => (base as usize, top as usize),
            _ => (frame_ref.rsp as usize, frame_ref.rsp as usize + 128),
        };

        for i in 0..16isize {
            match kdiag_stack_word_at(frame_ref.rsp, i, stack_lo, stack_hi) {
                Some(val) => {
                    klog_info!("  [RSP+0x{:02x}] = 0x{:016x}", (i as usize) * 8, val);
                }
                None => {
                    klog_info!(
                        "  [RSP+0x{:02x}] = <out of bounds, remaining omitted>",
                        (i as usize) * 8
                    );
                    break;
                }
            }
        }
        klog_info!("=== END STACK DUMP ===");

        // Also dump the current task context and CR3
        let current_cr3 = cpu::read_cr3();
        klog_info!("Active CR3: 0x{:x}", current_cr3);

        if let (Some(tid), Some(name_bytes), Some((kbase, ktop)), Some(fl), Some(ctx_cr3)) = (
            task_id_of(task_ptr),
            task_name_bytes(task_ptr),
            task_kernel_stack_bounds(task_ptr),
            task_flags(task_ptr),
            task_context_cr3(task_ptr),
        ) {
            klog_info!(
                "Current task: id={} name='{}' kstack=0x{:x}..0x{:x} flags=0x{:x} ctx_cr3=0x{:x}",
                tid,
                slopos_utils::string::bytes_as_str(name_bytes),
                kbase,
                ktop,
                fl,
                ctx_cr3
            );
            // Check if RSP is outside the task's kernel stack bounds
            let rsp_val = frame_ref.rsp;
            if rsp_val < kbase || rsp_val >= ktop {
                klog_info!("WARNING: RSP 0x{:x} OUTSIDE kernel stack bounds!", rsp_val);
            }
        }

        // Dump the kernel PML4 physical address for comparison
        if let Some(slot) = slopos_kernel_services::kernel_vm_space::try_kernel_vm_space() {
            let kd_phys = slot.lock().pml4_paddr().as_u64();
            klog_info!("Kernel CR3 (expected for kernel tasks): 0x{:x}", kd_phys);
        }
    }

    kdiag_dump_interrupt_frame(frame);
    panic_with_frame("Page fault", frame);
}

pub(crate) fn log_user_page_fault_diagnostics(frame_ref: &InterruptFrame, fault_addr: u64) {
    let mut pid = slopos_abi::task::INVALID_TASK_ID;
    let mut cr3 = 0u64;
    let mut fault_phys = PhysAddr::NULL;
    let mut rsp_phys = PhysAddr::NULL;
    let mut rip_phys = PhysAddr::NULL;
    let mut ctx_rip = 0u64;
    let mut ctx_rsp = 0u64;

    let task_ptr = resolve_user_fault_task();
    if let Some(task_pid) = task_process_id(task_ptr) {
        pid = task_pid;
        ctx_rip = task_context_rip(task_ptr).unwrap_or(0);
        ctx_rsp = task_context_rsp(task_ptr).unwrap_or(0);
        cr3 = process_vm::process_vm_get_ostd_pml4_paddr(pid);
        if cr3 != 0 {
            fault_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, fault_addr));
            rsp_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, frame_ref.rsp));
            rip_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, frame_ref.rip));
        }
    }

    if !rsp_phys.is_null() {
        if let Some(base_addr) = rsp_phys.to_virt_checked() {
            let base_u64 = base_addr.as_u64();
            // Read three words at the top of the user stack via the
            // HHDM-translated kernel virtual; bounds the read window
            // to a single page so a slot at the end of a page won't
            // wander into an unrelated mapping.
            let lo = base_u64 as usize;
            let hi = lo + 4096;
            let s0 = kdiag_stack_word_at(base_u64, 0, lo, hi).unwrap_or(0);
            let s1 = kdiag_stack_word_at(base_u64, 1, lo, hi).unwrap_or(0);
            let s2 = kdiag_stack_word_at(base_u64, 2, lo, hi).unwrap_or(0);
            klog_info!(
                "User PF stack top: [0]=0x{:x} [1]=0x{:x} [2]=0x{:x}",
                s0,
                s1,
                s2
            );
        } else {
            klog_info!(
                "User PF stack top unavailable (phys 0x{:x} unmapped)",
                rsp_phys.as_u64()
            );
        }
    }

    klog_info!(
        "User PF debug: pid={} cr3=0x{:x} fault_phys=0x{:x} rip_phys=0x{:x} rsp_phys=0x{:x}",
        pid,
        cr3,
        fault_phys.as_u64(),
        rip_phys.as_u64(),
        rsp_phys.as_u64()
    );
    klog_info!(
        "User PF context snapshot: rip=0x{:x} rsp=0x{:x}",
        ctx_rip,
        ctx_rsp
    );
}

pub(crate) fn is_critical_exception_internal(vector: u8) -> bool {
    slopos_arch::arch::exception::exception_is_critical(vector)
}
