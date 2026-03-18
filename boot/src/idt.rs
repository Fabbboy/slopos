#![allow(bad_asm_style)]

use core::arch::{asm, global_asm};
use core::cell::SyncUnsafeCell;
use core::ffi::{CStr, c_char, c_void};

use slopos_lib::cpu;
use slopos_lib::string::cstr_to_str;
use slopos_lib::{klog_debug, klog_info};

use crate::ist_stacks;
use crate::panic::set_panic_cpu_state;

global_asm!(include_str!("../idt_handlers.s"));

pub use slopos_lib::arch::idt::{
    EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_SEGMENT_NOT_PRES, EXCEPTION_SIMD_FP_EXCEPTION,
    EXCEPTION_STACK_FAULT, IDT_ENTRIES, IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IRQ_BASE_VECTOR,
    IdtEntry, LAPIC_TIMER_VECTOR, MSI_VECTOR_BASE, MSI_VECTOR_COUNT, RESCHEDULE_IPI_VECTOR,
    SYSCALL_VECTOR, TLB_SHOOTDOWN_VECTOR,
};

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

type ExceptionHandler = fn(*mut slopos_lib::InterruptFrame);

static IDT: SyncUnsafeCell<[IdtEntry; IDT_ENTRIES]> = SyncUnsafeCell::new(
    [IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    }; IDT_ENTRIES],
);

static IDT_POINTER: SyncUnsafeCell<IdtPtr> = SyncUnsafeCell::new(IdtPtr { limit: 0, base: 0 });

static PANIC_HANDLERS: SyncUnsafeCell<[ExceptionHandler; 32]> =
    SyncUnsafeCell::new([exception_default_panic; 32]);
static OVERRIDE_HANDLERS: SyncUnsafeCell<[Option<ExceptionHandler>; 32]> =
    SyncUnsafeCell::new([None; 32]);
static CURRENT_EXCEPTION_MODE: SyncUnsafeCell<ExceptionMode> =
    SyncUnsafeCell::new(ExceptionMode::Normal);

#[inline(always)]
fn handler_ptr(f: unsafe extern "C" fn()) -> u64 {
    f as *const () as u64
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

// Force Rust to recognize Idtr as used (it's used via IDT_POINTER static)
// Using size_of ensures the type is recognized as used at compile time
const _: usize = core::mem::size_of::<Idtr>();

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum ExceptionMode {
    Normal = 0,
    Test = 1,
}

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_core::irq::irq_dispatch;
use slopos_core::syscall::syscall_handle;
use slopos_drivers::apic::send_eoi;
use slopos_lib::kdiag_dump_interrupt_frame;
use slopos_mm::cow;
use slopos_mm::demand;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::tlb;
use slopos_mm::{paging, process_vm};

use slopos_core::sched::{
    RescheduleReason, TrapExitSource, schedule, scheduler_get_current_task,
    scheduler_handoff_on_trap_exit, scheduler_request_reschedule,
};
use slopos_core::scheduler::task::{task_find_by_cr3, task_pointer_is_valid};
use slopos_core::task::task_terminate;

use slopos_abi::task::{INVALID_PROCESS_ID, INVALID_TASK_ID, TaskExitReason, TaskFaultReason};
use slopos_core::scheduler::task_struct::Task;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

unsafe extern "C" {
    fn isr0();
    fn isr1();
    fn isr2();
    fn isr3();
    fn isr4();
    fn isr5();
    fn isr6();
    fn isr7();
    fn isr8();
    fn isr10();
    fn isr11();
    fn isr12();
    fn isr13();
    fn isr14();
    fn isr16();
    fn isr17();
    fn isr18();
    fn isr19();
    fn isr128();
    fn isr_reschedule_ipi();
    fn isr_tlb_shootdown();
    fn isr_shutdown_ipi();
    fn isr_spurious();
    fn isr_lapic_timer();

    fn irq0();
    fn irq1();
    fn irq2();
    fn irq3();
    fn irq4();
    fn irq5();
    fn irq6();
    fn irq7();
    fn irq8();
    fn irq9();
    fn irq10();
    fn irq11();
    fn irq12();
    fn irq13();
    fn irq14();
    fn irq15();

    /// Table of MSI vector stub entry-point addresses (vectors 48–223).
    /// Generated in `idt_handlers.s`; index i = address of stub for vector (48 + i).
    static msi_vector_table: [u64; MSI_VECTOR_COUNT];
}
pub fn idt_init() {
    klog_debug!("IDT: init start");
    unsafe {
        core::ptr::write_bytes(
            (*IDT.get()).as_mut_ptr() as *mut u8,
            0,
            core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>(),
        );
        (*IDT_POINTER.get()).limit = (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        (*IDT_POINTER.get()).base = (*IDT.get()).as_ptr() as u64;
    }

    idt_set_gate(0, handler_ptr(isr0), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(1, handler_ptr(isr1), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(2, handler_ptr(isr2), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(3, handler_ptr(isr3), 0x08, IDT_GATE_TRAP);
    idt_set_gate(4, handler_ptr(isr4), 0x08, IDT_GATE_TRAP);
    idt_set_gate(5, handler_ptr(isr5), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(6, handler_ptr(isr6), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(7, handler_ptr(isr7), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(8, handler_ptr(isr8), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(10, handler_ptr(isr10), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(11, handler_ptr(isr11), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(12, handler_ptr(isr12), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(13, handler_ptr(isr13), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(14, handler_ptr(isr14), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(16, handler_ptr(isr16), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(17, handler_ptr(isr17), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(18, handler_ptr(isr18), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(19, handler_ptr(isr19), 0x08, IDT_GATE_INTERRUPT);

    idt_set_gate(32, handler_ptr(irq0), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(33, handler_ptr(irq1), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(34, handler_ptr(irq2), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(35, handler_ptr(irq3), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(36, handler_ptr(irq4), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(37, handler_ptr(irq5), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(38, handler_ptr(irq6), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(39, handler_ptr(irq7), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(40, handler_ptr(irq8), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(41, handler_ptr(irq9), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(42, handler_ptr(irq10), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(43, handler_ptr(irq11), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(44, handler_ptr(irq12), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(45, handler_ptr(irq13), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(46, handler_ptr(irq14), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(47, handler_ptr(irq15), 0x08, IDT_GATE_INTERRUPT);

    idt_set_gate_priv(SYSCALL_VECTOR, handler_ptr(isr128), 0x08, IDT_GATE_TRAP, 3);

    idt_set_gate(
        RESCHEDULE_IPI_VECTOR,
        handler_ptr(isr_reschedule_ipi),
        0x08,
        IDT_GATE_INTERRUPT,
    );
    idt_set_gate(
        TLB_SHOOTDOWN_VECTOR,
        handler_ptr(isr_tlb_shootdown),
        0x08,
        IDT_GATE_INTERRUPT,
    );
    idt_set_gate(
        0xFE,
        handler_ptr(isr_shutdown_ipi),
        0x08,
        IDT_GATE_INTERRUPT,
    );
    idt_set_gate(0xFF, handler_ptr(isr_spurious), 0x08, IDT_GATE_INTERRUPT);
    idt_set_gate(
        LAPIC_TIMER_VECTOR,
        handler_ptr(isr_lapic_timer),
        0x08,
        IDT_GATE_INTERRUPT,
    );

    // MSI interrupt vectors (48–223): install stubs from the assembly-generated table.
    // Skip vectors that have dedicated handlers (e.g. SYSCALL_VECTOR = 0x80).
    unsafe {
        for i in 0..MSI_VECTOR_COUNT {
            let vector = MSI_VECTOR_BASE.wrapping_add(i as u8);
            if vector == SYSCALL_VECTOR {
                continue;
            }
            idt_set_gate(vector, msi_vector_table[i], 0x08, IDT_GATE_INTERRUPT);
        }
    }
    klog_debug!(
        "IDT: Installed {} MSI vector stubs (vectors {}-{})",
        MSI_VECTOR_COUNT,
        MSI_VECTOR_BASE,
        MSI_VECTOR_BASE as usize + MSI_VECTOR_COUNT - 1
    );

    initialize_handler_tables();

    klog_debug!("IDT: Configured 256 interrupt vectors");
    let base = unsafe { (*IDT_POINTER.get()).base };
    let limit = unsafe { (*IDT_POINTER.get()).limit };
    klog_debug!("IDT: init prepared base=0x{:x} limit=0x{:x}", base, limit);
}
pub fn idt_set_gate_priv(vector: u8, handler: u64, selector: u16, typ: u8, dpl: u8) {
    unsafe {
        (*IDT.get())[vector as usize].offset_low = (handler & 0xFFFF) as u16;
        (*IDT.get())[vector as usize].selector = selector;
        (*IDT.get())[vector as usize].ist = 0;
        (*IDT.get())[vector as usize].type_attr = typ | 0x80 | ((dpl & 0x3) << 5);
        (*IDT.get())[vector as usize].offset_mid = ((handler >> 16) & 0xFFFF) as u16;
        (*IDT.get())[vector as usize].offset_high = (handler >> 32) as u32;
        (*IDT.get())[vector as usize].zero = 0;
    }
}
pub fn idt_set_gate(vector: u8, handler: u64, selector: u16, typ: u8) {
    idt_set_gate_priv(vector, handler, selector, typ, 0);
}
pub fn idt_get_gate(vector: u8, out_entry: *mut IdtEntry) -> i32 {
    if out_entry.is_null() || vector as usize >= IDT_ENTRIES {
        return -1;
    }
    unsafe {
        *out_entry = (*IDT.get())[vector as usize];
    }
    0
}

pub fn idt_get_gate_opaque(vector: u8, out_entry: *mut c_void) -> i32 {
    idt_get_gate(vector, out_entry as *mut IdtEntry)
}
pub fn idt_install_exception_handler(vector: u8, handler: ExceptionHandler) {
    if vector >= 32 {
        klog_info!(
            "IDT: Ignoring handler install for non-exception vector {}",
            vector
        );
        return;
    }
    if is_critical_exception_internal(vector) {
        klog_info!("IDT: Refusing to override critical exception {}", vector);
        return;
    }
    unsafe {
        (*OVERRIDE_HANDLERS.get())[vector as usize] = Some(handler);
        klog_debug!("IDT: Registered override handler for exception {}", vector);
    }
}
pub fn idt_set_ist(vector: u8, ist_index: u8) {
    if vector as usize >= IDT_ENTRIES {
        klog_info!("IDT: Invalid IST assignment for vector {}", vector);
        return;
    }
    if ist_index > 7 {
        klog_info!("IDT: Invalid IST index {}", ist_index);
        return;
    }

    unsafe {
        (*IDT.get())[vector as usize].ist = ist_index & 0x7;
    }
}
pub fn exception_set_mode(mode: ExceptionMode) {
    unsafe {
        *CURRENT_EXCEPTION_MODE.get() = mode;
        if let ExceptionMode::Normal = mode {
            *OVERRIDE_HANDLERS.get() = [None; 32];
        }
    }
}
pub fn exception_is_critical(vector: u8) -> i32 {
    slopos_lib::arch::exception::exception_is_critical(vector) as i32
}
pub fn idt_load() {
    unsafe {
        (*IDT_POINTER.get()).limit = (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        (*IDT_POINTER.get()).base = (*IDT.get()).as_ptr() as u64;
        let idtr = IDT_POINTER.get() as *const IdtPtr;
        asm!("lidt [{}]", in(reg) idtr, options(nostack, preserves_flags));
    }
}

fn handle_tlb_shootdown_ipi() {
    let apic_id = slopos_drivers::apic::get_id();
    if let Some(cpu_idx) = slopos_lib::cpu_index_from_apic_id(apic_id) {
        tlb::handle_shootdown_ipi(cpu_idx);
    } else {
        klog_debug!(
            "TLB: Missing CPU index for APIC 0x{:x}; cannot ack shootdown",
            apic_id
        );
    }
    send_eoi();
}

/// RAII guard that holds preempt_count elevated without triggering the
/// reschedule callback on drop.  Used for IST-based exception handlers
/// where yielding would leave the handler suspended on a reusable IST stack.
struct IstPreemptHold {
    active: bool,
}

impl IstPreemptHold {
    /// Increment preempt_count to prevent deferred rescheduling.
    #[inline]
    fn new(active: bool) -> Self {
        if active {
            unsafe {
                slopos_lib::pcr::current_pcr()
                    .preempt_count
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
        Self { active }
    }
}

impl Drop for IstPreemptHold {
    #[inline]
    fn drop(&mut self) {
        if self.active {
            // Decrement WITHOUT calling the reschedule callback.
            // Any pending reschedule will be handled naturally by the next
            // timer tick or voluntary yield after we return via IRET.
            unsafe {
                slopos_lib::pcr::current_pcr()
                    .preempt_count
                    .fetch_sub(1, core::sync::atomic::Ordering::Release);
            }
        }
    }
}

/// Implementation of common_exception_handler - called from FFI boundary
pub fn common_exception_handler_impl(frame: *mut slopos_lib::InterruptFrame) {
    let frame_ref = unsafe { &mut *frame };
    let vector = (frame_ref.vector & 0xFF) as u8;

    // Prevent deferred rescheduling during IST-based exception handlers.
    //
    // IST stacks are per-vector, per-CPU fixed addresses.  If an IrqMutex guard
    // drops while preempt_count is 1, the PreemptGuard::drop callback will call
    // the scheduler, context-switching away from the IST stack.  A subsequent
    // exception of the same vector would reuse the same IST stack, overwriting
    // the suspended handler's state → corruption / triple fault.
    //
    // By bumping preempt_count here (and manually decrementing on exit WITHOUT
    // calling the reschedule callback), we ensure all inner IrqMutex drops see
    // preempt_count > 1 and skip the callback.
    //
    // All CPU exceptions (vectors 0-31) use IST stacks in SlopOS.
    let _ist_hold = IstPreemptHold::new(vector < 32);

    ist_stacks::ist_record_usage(vector, frame as u64);

    if vector == SYSCALL_VECTOR {
        syscall_handle(frame);
        return;
    }

    if vector == TLB_SHOOTDOWN_VECTOR {
        handle_tlb_shootdown_ipi();
        return;
    }

    if vector == RESCHEDULE_IPI_VECTOR {
        send_eoi();
        scheduler_request_reschedule(RescheduleReason::RescheduleIpi);
        scheduler_handoff_on_trap_exit(TrapExitSource::RescheduleIpi);
        return;
    }

    if vector == 0xFE {
        send_eoi();
        cpu::disable_interrupts();
        cpu::halt_loop();
    }

    // LAPIC timer: per-CPU preemption tick — handled directly, not through
    // the IOAPIC IRQ dispatch table.  Each CPU has its own LAPIC timer.
    if vector == LAPIC_TIMER_VECTOR {
        slopos_core::irq::increment_timer_ticks();
        slopos_core::sched::scheduler_handle_timer_interrupt(frame);
        send_eoi();
        scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
        return;
    }

    if vector >= IRQ_BASE_VECTOR {
        irq_dispatch(frame);
        return;
    }

    if vector == EXCEPTION_PAGE_FAULT {
        if try_handle_page_fault(frame) {
            return;
        }
    }

    let cr2 = cpu::read_cr2();
    klog_debug!(
        "EXCEPTION: vec={} rip=0x{:x} err=0x{:x} cs=0x{:x} ss=0x{:x} cr2=0x{:x}",
        vector,
        frame_ref.rip,
        frame_ref.error_code,
        frame_ref.cs,
        frame_ref.ss,
        cr2
    );

    if vector >= 32 {
        klog_info!("EXCEPTION: Unknown vector {}", vector);
        exception_default_panic(frame);
        return;
    }

    let critical = is_critical_exception_internal(vector);
    unsafe {
        if critical || !matches!(*CURRENT_EXCEPTION_MODE.get(), ExceptionMode::Test) {
            let name = slopos_lib::arch::exception::get_exception_name(vector);
            klog_info!("EXCEPTION: Vector {} ({})", vector, name);
        }
    }

    let mut handler = unsafe { (*PANIC_HANDLERS.get())[vector as usize] };
    if !critical
        && matches!(
            unsafe { *CURRENT_EXCEPTION_MODE.get() },
            ExceptionMode::Test
        )
    {
        if let Some(override_handler) = unsafe { (*OVERRIDE_HANDLERS.get())[vector as usize] } {
            handler = override_handler;
        }
    }

    handler(frame);
}
fn initialize_handler_tables() {
    unsafe {
        *PANIC_HANDLERS.get() = [exception_default_panic; 32];
        *OVERRIDE_HANDLERS.get() = [None; 32];

        // Fatal: log name, dump frame, panic.
        (*PANIC_HANDLERS.get())[EXCEPTION_DIVIDE_ERROR as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_NMI as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_DOUBLE_FAULT as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_INVALID_TSS as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_SEGMENT_NOT_PRES as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_STACK_FAULT as usize] = exception_fatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_MACHINE_CHECK as usize] = exception_fatal;

        // Non-fatal: log name, dump frame, resume.
        (*PANIC_HANDLERS.get())[EXCEPTION_DEBUG as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_BREAKPOINT as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_OVERFLOW as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_BOUND_RANGE as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_FPU_ERROR as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_ALIGNMENT_CHECK as usize] = exception_nonfatal;
        (*PANIC_HANDLERS.get())[EXCEPTION_SIMD_FP_EXCEPTION as usize] = exception_nonfatal;

        // Specialized: user-mode check before fatal/nonfatal fallback.
        (*PANIC_HANDLERS.get())[EXCEPTION_INVALID_OPCODE as usize] = exception_invalid_opcode;
        (*PANIC_HANDLERS.get())[EXCEPTION_DEVICE_NOT_AVAIL as usize] =
            exception_device_not_available;
        (*PANIC_HANDLERS.get())[EXCEPTION_GENERAL_PROTECTION as usize] =
            exception_general_protection;
        (*PANIC_HANDLERS.get())[EXCEPTION_PAGE_FAULT as usize] = exception_page_fault;
    }
}

fn is_critical_exception_internal(vector: u8) -> bool {
    slopos_lib::arch::exception::exception_is_critical(vector)
}

fn in_user(frame: &slopos_lib::InterruptFrame) -> bool {
    (frame.cs & 0x3) == 0x3
}

fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
    // SAFETY: All call sites provide statically defined, NUL-terminated byte
    // strings so this conversion cannot fail at runtime.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

#[inline]
fn resolve_user_fault_task() -> *mut Task {
    let hw_cr3 = cpu::read_cr3() & !0xFFF;
    let mut task = scheduler_get_current_task() as *mut Task;

    if !task.is_null() && task_pointer_is_valid(task as *const Task) {
        let task_cr3 =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.cr3)) } & !0xFFF;
        if task_cr3 == hw_cr3 {
            return task;
        }
    } else {
        task = core::ptr::null_mut();
    }

    let by_cr3 = task_find_by_cr3(hw_cr3);
    if !by_cr3.is_null() {
        return by_cr3;
    }

    task
}

fn terminate_user_task(
    reason: TaskFaultReason,
    frame: &slopos_lib::InterruptFrame,
    detail: &'static CStr,
) {
    let task = resolve_user_fault_task();

    if task.is_null() {
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
    }

    let tid = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };
    let detail_str = detail.to_str().unwrap_or("<invalid utf-8>");
    let cr2 = cpu::read_cr2();
    let (rip, rsp, vec, err) = (frame.rip, frame.rsp, frame.vector, frame.error_code);
    let (entry_point, proc_id, flags, name_str) = if task.is_null() {
        (0, 0, 0, "<no task>")
    } else {
        let name_raw = unsafe { &(*task).name };
        let len = name_raw
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_raw.len());
        let name = core::str::from_utf8(&name_raw[..len]).unwrap_or("<invalid utf-8>");
        let ep = unsafe { (*task).entry_point };
        let pid = unsafe { (*task).process_id };
        let fl = unsafe { (*task).flags };
        (ep, pid, fl, name)
    };
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
    if !task.is_null() {
        unsafe {
            (*task).exit_reason = TaskExitReason::UserFault;
            (*task).fault_reason = reason;
            (*task).exit_code = 1;
            task_terminate(tid);
            // CRITICAL: Call schedule() directly instead of just setting pending flag.
            // If we return from this exception handler, iretq will try to resume the
            // faulting instruction, causing an infinite loop of faults.
            // schedule() will switch to another task and never return here.
            schedule();
        }
    }
    let _ = frame;
}

fn exception_default_panic(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Unhandled exception");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame("Unhandled exception", frame);
}

fn exception_fatal(frame: *mut slopos_lib::InterruptFrame) {
    let name = frame_exception_name(frame);
    klog_info!("FATAL: {}", name);
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(name, frame);
}

fn exception_nonfatal(frame: *mut slopos_lib::InterruptFrame) {
    let name = frame_exception_name(frame);
    klog_info!("ERROR: {}", name);
    kdiag_dump_interrupt_frame(frame);
}

fn frame_exception_name(frame: *mut slopos_lib::InterruptFrame) -> &'static str {
    let vector = (unsafe { &*frame }.vector & 0xFF) as u8;
    slopos_lib::arch::exception::get_exception_name(vector)
}

fn exception_invalid_opcode(frame: *mut slopos_lib::InterruptFrame) {
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

fn exception_device_not_available(frame: *mut slopos_lib::InterruptFrame) {
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

fn exception_general_protection(frame: *mut slopos_lib::InterruptFrame) {
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
/// Attempt to resolve a page fault via CoW or demand paging.
///
/// This is the **single authority** for recoverable user-space page fault
/// resolution.  It is called from `common_exception_handler_impl` before the
/// exception handler dispatch; a `true` return means the fault was resolved
/// in-place and execution can resume.
///
/// Returns `false` for any non-recoverable case (kernel faults, IST guard
/// hits, missing task/page-dir, or failed resolution) — the caller must then
/// fall through to the diagnostic / terminate / panic path in
/// `exception_page_fault`.
fn try_handle_page_fault(frame: *mut slopos_lib::InterruptFrame) -> bool {
    let fault_addr = cpu::read_cr2();
    let frame_ref = unsafe { &*frame };

    // IST guard page hit → not recoverable here (diagnosed later).
    if ist_stacks::ist_guard_fault(fault_addr, core::ptr::null_mut()) != 0 {
        return false;
    }

    // Kernel-mode faults are never transparently resolved.
    if !in_user(frame_ref) {
        return false;
    }

    let task_ptr = resolve_user_fault_task();
    if task_ptr.is_null() {
        return false;
    }

    let pid = unsafe { (*task_ptr).process_id };
    if pid == INVALID_PROCESS_ID || (pid as usize) >= MAX_PROCESSES {
        return false;
    }
    let tid = unsafe { (*task_ptr).task_id };
    let page_dir = process_vm::process_vm_get_page_dir(pid);
    if page_dir.is_null() || (page_dir as u64) < 0xffff_8000_0000_0000 {
        return false;
    }

    // Copy-on-Write resolution.
    if cow::is_cow_fault(frame_ref.error_code, page_dir, fault_addr) {
        klog_debug!(
            "PF: COW fault task {} (pid {}) at cr2=0x{:x} err=0x{:x} rip=0x{:x}",
            tid,
            pid,
            fault_addr,
            frame_ref.error_code,
            frame_ref.rip
        );
        let result = cow::handle_cow_fault(page_dir, fault_addr);

        if result.is_ok() {
            klog_debug!(
                "PF: COW resolved for task {} at cr2=0x{:x}",
                tid,
                fault_addr
            );
            return true;
        }
        klog_info!(
            "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
            tid,
            fault_addr
        );
    }

    // Demand paging resolution.
    if demand::is_demand_fault(frame_ref.error_code, pid, fault_addr) {
        if demand::handle_demand_fault(page_dir, pid, fault_addr, frame_ref.error_code).is_ok() {
            return true;
        }
    }

    false
}

/// Unrecoverable page fault handler.
///
/// By the time this runs, `try_handle_page_fault` has already attempted (and
/// failed) CoW and demand-paging resolution.  This function only performs
/// diagnostics and terminates the faulting context (user task or kernel panic).
fn exception_page_fault(frame: *mut slopos_lib::InterruptFrame) {
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

fn log_user_page_fault_diagnostics(frame_ref: &slopos_lib::InterruptFrame, fault_addr: u64) {
    let mut pid = INVALID_TASK_ID;
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

fn panic_with_frame(message: &str, frame: *mut slopos_lib::InterruptFrame) {
    let frame_ref = unsafe { &*frame };
    set_panic_cpu_state(frame_ref.rip, frame_ref.rsp);
    panic!("{}", message);
}
