use core::ffi::c_char;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::task::TaskFaultReason;
use slopos_lib::cpu;
use slopos_lib::string::cstr_to_str;
use slopos_lib::{InterruptFrame, kdiag_dump_interrupt_frame, klog_info};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::{paging, process_vm};

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
    slopos_lib::arch::exception::get_exception_name(vector)
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
        let dir = process_vm::process_vm_get_page_dir(pid);
        if !dir.is_null() {
            cr3 = unsafe { (*dir).pml4_phys.as_u64() };
            fault_phys = paging::virt_to_phys_process(VirtAddr::new(fault_addr), dir);
            rsp_phys = paging::virt_to_phys_process(VirtAddr::new(frame_ref.rsp), dir);
            rip_phys = paging::virt_to_phys_process(VirtAddr::new(frame_ref.rip), dir);
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
    slopos_lib::arch::exception::exception_is_critical(vector)
}
