use slopos_abi::addr::PhysAddr;
use slopos_abi::task::TaskFaultReason;
use slopos_arch::InterruptFrame;
use slopos_arch::cpu;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::process_vm;
use slopos_ostd::task::TaskDiag;
use slopos_ostd::{kdiag_dump_interrupt_frame, kdiag_stack_word_at, klog_info};

use crate::ist_stacks;
use crate::user_fault::*;

/// Per-CPU slot for why the last unresolvable user page fault could not be
/// serviced.
///
/// Written by the resolver and consumed by the handler two frames above it, on
/// the same CPU, with interrupts off and no blocking call between — an
/// exception handler cannot be re-entered by another fault on this CPU before
/// it reads the value back. Indexed by CPU anyway rather than shared, so a
/// concurrent fault on a *peer* cannot claim this one's reason.
///
/// Exists because the distinction is otherwise invisible: a task killed by a
/// wild pointer and one killed because the machine ran out of memory produce
/// the same `waitpid` status without it, and only one of them is the program's
/// fault.
static PENDING_FAULT_REASON: [core::sync::atomic::AtomicU16; slopos_arch::pcr::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU16::new(0) }; slopos_arch::pcr::MAX_CPUS];

/// Record why a fault could not be serviced. Called from the resolver.
pub(crate) fn record_fault_reason(reason: TaskFaultReason) {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if let Some(slot) = PENDING_FAULT_REASON.get(cpu) {
        slot.store(reason.as_u16(), core::sync::atomic::Ordering::Relaxed);
    }
}

/// Take this CPU's recorded reason, leaving `None` behind.
///
/// Consuming rather than peeking: a stale reason would attribute the *next*
/// wild pointer on this CPU to an out-of-memory condition.
fn take_fault_reason() -> TaskFaultReason {
    let cpu = slopos_arch::pcr::get_current_cpu();
    let raw = PENDING_FAULT_REASON.get(cpu).map_or(0, |slot| {
        slot.swap(0, core::sync::atomic::Ordering::Relaxed)
    });
    TaskFaultReason::from_u16(raw)
}

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
    // The frame lives for exactly this handler invocation, so a frame-local
    // is the honest anchor for a borrow of it.
    let frame_anchor = ();
    let vector = match InterruptFrame::from_ptr(&frame_anchor, frame) {
        Some(f) => (f.vector & 0xFF) as u8,
        None => return "<null frame>",
    };
    slopos_arch::arch::exception::get_exception_name(vector)
}

pub(crate) fn exception_invalid_opcode(frame: *mut InterruptFrame) {
    // The frame lives for exactly this handler invocation, so a frame-local
    // is the honest anchor for a borrow of it.
    let frame_anchor = ();
    if let Some(f) = InterruptFrame::from_ptr(&frame_anchor, frame) {
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
    // The frame lives for exactly this handler invocation, so a frame-local
    // is the honest anchor for a borrow of it.
    let frame_anchor = ();
    if let Some(f) = InterruptFrame::from_ptr(&frame_anchor, frame) {
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
    // The frame lives for exactly this handler invocation, so a frame-local
    // is the honest anchor for a borrow of it.
    let frame_anchor = ();
    if let Some(f) = InterruptFrame::from_ptr(&frame_anchor, frame) {
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
    // The frame lives for exactly this handler invocation, so a frame-local
    // is the honest anchor for a borrow of it.
    let frame_anchor = ();
    let fault_addr = cpu::read_cr2();
    let Some(frame_ref) = InterruptFrame::from_ptr(&frame_anchor, frame) else {
        klog_info!("FATAL: page fault with null frame pointer");
        panic!("page fault with null frame");
    };

    if let Some(stack_name) = ist_stacks::ist_guard_fault(fault_addr) {
        klog_info!("FATAL: IST stack overflow detected via guard page");
        klog_info!("Stack: {}", slopos_ostd::string::bytes_as_str(stack_name));
        klog_info!("Fault address: 0x{:x}", fault_addr);
        kdiag_dump_interrupt_frame(frame);
        panic_with_frame("IST stack overflow", frame);
        return;
    }

    // Exception SafeStack DATA-stack overflow: a per-CPU exception data stack
    // grew past its guard page. This is the one fault we MUST report without
    // `format_args!` — the normal diagnostic/panic path builds its Argument
    // array on the exception data stack, i.e. the very stack that just
    // overflowed, so it would re-fault. Route to the format-free abort.
    if ist_stacks::exc_dstack_guard_fault(fault_addr).is_some() {
        crate::panic::panic_abort_raw(
            "exception data-stack overflow: a CPU exhausted its per-CPU IST/exception SafeStack data stack",
        );
    }

    // Reliable Abort Core emergency-stack overflow: the fatal-fault reporter
    // itself exhausted its emergency SAFE or DATA stack. The #PF lands here on
    // a fresh IST page-fault stack (RSP in the IST region → `ist_unsafe_sp`),
    // so the format-free abort has a usable data stack. Degrade to it rather
    // than recursing through the (overflowed) reporter.
    if ist_stacks::emergency_stack_guard_fault(fault_addr).is_some() {
        crate::panic::panic_abort_raw(
            "emergency stack overflow: the fatal-fault reporter exhausted its per-CPU emergency stack",
        );
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
        let (reason, detail) = match take_fault_reason() {
            TaskFaultReason::UserOom => (
                TaskFaultReason::UserOom,
                cstr_from_bytes(b"out of memory servicing a user page fault\0"),
            ),
            _ => (
                TaskFaultReason::UserPage,
                cstr_from_bytes(b"user page fault\0"),
            ),
        };
        terminate_user_task(reason, frame_ref, detail);
        return;
    }

    // Supervisor instruction-fetch fault — dump stack words around RSP
    // to help diagnose which return address was corrupted.
    if (frame_ref.error_code & 0x10) != 0 {
        klog_info!("=== STACK DUMP at RSP 0x{:x} ===", frame_ref.rsp);

        // One racy snapshot, reused for the probe bounds and the task line
        // below. This is a fault path: it must take no lock (every registry
        // lookup holds the global cli-spinlock, and a fault arriving while a
        // CPU holds it would hang the dump) and must upgrade no `KArc` (the
        // matching drop could run the allocator-heavy destructor from a fault
        // handler on an IST stack). `current_task_diag` does neither — see its
        // module docs for the residual hazard it accepts and why.
        let diag = slopos_sched::task_struct::current_task_diag();
        let (stack_lo, stack_hi) =
            TaskDiag::probe_range(diag.as_ref(), frame_ref.rsp as usize, 128);

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

        if let Some(diag) = diag.as_ref() {
            klog_info!(
                "Current task: id={} name='{}' kstack=0x{:x}..0x{:x} flags=0x{:x} ctx_cr3=0x{:x}",
                diag.id,
                diag.name_str(),
                diag.kernel_stack_base,
                diag.kernel_stack_top,
                diag.flags,
                diag.context_cr3
            );
            if !diag.stack_contains(frame_ref.rsp) {
                klog_info!(
                    "WARNING: RSP 0x{:x} OUTSIDE kernel stack bounds!",
                    frame_ref.rsp
                );
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

    let task_ref = resolve_user_fault_task();
    // Not the panic path: a user page fault is recoverable, and
    // `resolve_user_fault_task` deliberately keeps its registry lookup. The
    // guard derefs to `&Task`, so the fields come off it directly rather than
    // through a raw projection.
    if let Some(task_ref) = task_ref {
        let task_pid = task_ref.process_id;
        // The faulting task's own process, not a lookup by its id: this is a
        // diagnostic path, and reporting another process's physical addresses
        // because the number was reissued would be worse than reporting none.
        let faulting = task_ref
            .process()
            .as_deref()
            .and_then(slopos_ostd::process::ProcessId::of);
        if task_pid != slopos_abi::task::INVALID_TASK_ID
            && let Some(faulting) = faulting
        {
            pid = task_pid;
            ctx_rip = task_ref.context_rip();
            ctx_rsp = task_ref.context_rsp();
            cr3 = process_vm::process_vm_get_ostd_pml4_paddr(faulting);
            if cr3 != 0 {
                fault_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(
                    faulting, fault_addr,
                ));
                rsp_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(
                    faulting,
                    frame_ref.rsp,
                ));
                rip_phys = PhysAddr::new(process_vm::process_vm_user_va_to_paddr(
                    faulting,
                    frame_ref.rip,
                ));
            }
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
