use core::ffi::c_char;

use slopos_abi::addr::PhysAddr;
use slopos_abi::task::TaskFaultReason;
use slopos_arch::InterruptFrame;
use slopos_arch::cpu;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::process_vm;
use slopos_utils::string::cstr_to_str;
use slopos_utils::{kdiag_dump_interrupt_frame, klog_info};

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
    let vector = (unsafe { &*frame }.vector & 0xFF) as u8;
    slopos_arch::arch::exception::get_exception_name(vector)
}

pub(crate) fn exception_invalid_opcode(frame: *mut InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserUd,
            unsafe { &*frame },
            cstr_from_bytes(b"invalid opcode in user mode\0"),
        );
        return;
    }
    exception_fatal(frame);
}

pub(crate) fn exception_device_not_available(frame: *mut InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserDeviceNa,
            unsafe { &*frame },
            cstr_from_bytes(b"device not available in user mode\0"),
        );
        return;
    }
    exception_nonfatal(frame);
}

pub(crate) fn exception_general_protection(frame: *mut InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserGp,
            unsafe { &*frame },
            cstr_from_bytes(b"general protection from user mode\0"),
        );
        return;
    }
    exception_fatal(frame);
}

pub(crate) fn exception_page_fault(frame: *mut InterruptFrame) {
    let fault_addr = cpu::read_cr2();
    let frame_ref = unsafe { &*frame };

    let mut stack_name: *const c_char = core::ptr::null();
    if ist_stacks::ist_guard_fault(fault_addr, &mut stack_name) != 0 {
        klog_info!("FATAL: IST stack overflow detected via guard page");
        if !stack_name.is_null() {
            klog_info!("Stack: {}", unsafe { cstr_to_str(stack_name) });
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
        let rsp = frame_ref.rsp as *const u64;

        // Determine a safe probe range for the stack dump.  Call
        // scheduler_get_current_task() once and reuse the pointer for
        // both bounds computation and the task-context dump below.
        let task_ptr = slopos_core::sched::scheduler_get_current_task();
        let (stack_lo, stack_hi) = if !task_ptr.is_null() {
            let base = unsafe { (*task_ptr).kernel_stack_base };
            let top = unsafe { (*task_ptr).kernel_stack_top };
            if base != 0 && top > base {
                (base as usize, top as usize)
            } else {
                (frame_ref.rsp as usize, frame_ref.rsp as usize + 128)
            }
        } else {
            (frame_ref.rsp as usize, frame_ref.rsp as usize + 128)
        };

        for i in 0..16isize {
            let addr = unsafe { rsp.offset(i) };
            let addr_val = addr as usize;
            if addr_val < stack_lo || addr_val + 8 > stack_hi {
                klog_info!(
                    "  [RSP+0x{:02x}] = <out of bounds, remaining omitted>",
                    (i as usize) * 8
                );
                break;
            }
            let val = unsafe { core::ptr::read_unaligned(addr) };
            klog_info!("  [RSP+0x{:02x}] = 0x{:016x}", (i as usize) * 8, val);
        }
        klog_info!("=== END STACK DUMP ===");

        // Also dump the current task context and CR3
        let current_cr3 = cpu::read_cr3();
        klog_info!("Active CR3: 0x{:x}", current_cr3);

        if !task_ptr.is_null() {
            unsafe {
                let ctx_cr3 =
                    core::ptr::read_unaligned(core::ptr::addr_of!((*task_ptr).context.cr3));
                klog_info!(
                    "Current task: id={} name='{}' kstack=0x{:x}..0x{:x} flags=0x{:x} ctx_cr3=0x{:x}",
                    (*task_ptr).task_id,
                    slopos_utils::string::bytes_as_str(&(*task_ptr).name),
                    (*task_ptr).kernel_stack_base,
                    (*task_ptr).kernel_stack_top,
                    (*task_ptr).flags,
                    ctx_cr3
                );
                // Check if RSP is outside the task's kernel stack bounds
                let rsp_val = frame_ref.rsp;
                if rsp_val < (*task_ptr).kernel_stack_base
                    || rsp_val >= (*task_ptr).kernel_stack_top
                {
                    klog_info!("WARNING: RSP 0x{:x} OUTSIDE kernel stack bounds!", rsp_val);
                }
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
    if !task_ptr.is_null() {
        pid = unsafe { (*task_ptr).process_id };
        unsafe {
            ctx_rip = core::ptr::read_unaligned(core::ptr::addr_of!((*task_ptr).context.rip));
            ctx_rsp = core::ptr::read_unaligned(core::ptr::addr_of!((*task_ptr).context.rsp));
        }
        cr3 = process_vm::process_vm_get_ostd_pml4_paddr(pid);
        if cr3 != 0 {
            fault_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, fault_addr));
            rsp_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, frame_ref.rsp));
            rip_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(pid, frame_ref.rip));
        }
    }

    if !rsp_phys.is_null() {
        if let Some(base_addr) = rsp_phys.to_virt_checked() {
            let base = base_addr.as_u64() as *const u64;
            unsafe {
                let s0 = core::ptr::read_unaligned(base);
                let s1 = core::ptr::read_unaligned(base.add(1));
                let s2 = core::ptr::read_unaligned(base.add(2));
                klog_info!(
                    "User PF stack top: [0]=0x{:x} [1]=0x{:x} [2]=0x{:x}",
                    s0,
                    s1,
                    s2
                );
            }
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
