#![allow(static_mut_refs)]
#![allow(bad_asm_style)]

use core::arch::{asm, global_asm};
use core::ffi::{CStr, c_char, c_void};

use slopos_drivers::serial_println;
use slopos_lib::string::cstr_to_str;
use slopos_lib::{klog_debug, klog_info};

use crate::kernel_panic::kernel_panic_with_state;
use crate::safe_stack;

global_asm!(include_str!("../idt_handlers.s"));

// Import IDT constants from abi
pub use slopos_abi::arch::x86_64::idt::{
    EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_SEGMENT_NOT_PRES, EXCEPTION_SIMD_FP_EXCEPTION,
    EXCEPTION_STACK_FAULT, IDT_ENTRIES, IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IRQ_BASE_VECTOR,
    SYSCALL_VECTOR,
};

// IdtEntry is now imported from abi
pub use slopos_abi::task::IdtEntry;

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

type ExceptionHandler = fn(*mut slopos_lib::InterruptFrame);

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry {
    offset_low: 0,
    selector: 0,
    ist: 0,
    type_attr: 0,
    offset_mid: 0,
    offset_high: 0,
    zero: 0,
}; IDT_ENTRIES];

static mut IDT_POINTER: IdtPtr = IdtPtr { limit: 0, base: 0 };

static mut PANIC_HANDLERS: [ExceptionHandler; 32] = [exception_default_panic; 32];
static mut OVERRIDE_HANDLERS: [Option<ExceptionHandler>; 32] = [None; 32];
static mut CURRENT_EXCEPTION_MODE: ExceptionMode = ExceptionMode::Normal;

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
use slopos_core::wl_currency::wl_award_loss;
use slopos_drivers::syscall::syscall_handle;
use slopos_lib::kdiag_dump_interrupt_frame;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::{paging, process_vm};

use slopos_core::{
    scheduler_get_current_task, scheduler_request_reschedule_from_interrupt, task_terminate,
};

// Task and related types are now imported from abi
use slopos_abi::task::{INVALID_TASK_ID, Task, TaskExitReason, TaskFaultReason};

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
}
pub fn idt_init() {
    serial_println!("IDT: init start");
    unsafe {
        core::ptr::write_bytes(
            IDT.as_mut_ptr() as *mut u8,
            0,
            core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>(),
        );
        IDT_POINTER.limit = (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        IDT_POINTER.base = IDT.as_ptr() as u64;
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

    initialize_handler_tables();

    klog_debug!("IDT: Configured 256 interrupt vectors");
    let base = unsafe { IDT_POINTER.base };
    let limit = unsafe { IDT_POINTER.limit };
    serial_println!("IDT: init prepared base=0x{:x} limit=0x{:x}", base, limit);
}
pub fn idt_set_gate_priv(vector: u8, handler: u64, selector: u16, typ: u8, dpl: u8) {
    unsafe {
        IDT[vector as usize].offset_low = (handler & 0xFFFF) as u16;
        IDT[vector as usize].selector = selector;
        IDT[vector as usize].ist = 0;
        IDT[vector as usize].type_attr = typ | 0x80 | ((dpl & 0x3) << 5);
        IDT[vector as usize].offset_mid = ((handler >> 16) & 0xFFFF) as u16;
        IDT[vector as usize].offset_high = (handler >> 32) as u32;
        IDT[vector as usize].zero = 0;
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
        *out_entry = IDT[vector as usize];
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
        OVERRIDE_HANDLERS[vector as usize] = Some(handler);
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
        IDT[vector as usize].ist = ist_index & 0x7;
    }
}
pub fn exception_set_mode(mode: ExceptionMode) {
    unsafe {
        CURRENT_EXCEPTION_MODE = mode;
        if let ExceptionMode::Normal = mode {
            OVERRIDE_HANDLERS = [None; 32];
        }
    }
}
pub fn exception_is_critical(vector: u8) -> i32 {
    is_critical_exception_internal(vector) as i32
}
pub fn idt_load() {
    unsafe {
        IDT_POINTER.limit = (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        IDT_POINTER.base = IDT.as_ptr() as u64;
        let idtr = &raw const IDT_POINTER;
        asm!("lidt [{}]", in(reg) idtr, options(nostack, preserves_flags));
    }
}

/// Implementation of common_exception_handler - called from FFI boundary
pub fn common_exception_handler_impl(frame: *mut slopos_lib::InterruptFrame) {
    let frame_ref = unsafe { &mut *frame };
    let vector = (frame_ref.vector & 0xFF) as u8;

    safe_stack::safe_stack_record_usage(vector, frame as u64);

    if vector == SYSCALL_VECTOR {
        syscall_handle(frame);
        return;
    }

    if vector >= IRQ_BASE_VECTOR {
        irq_dispatch(frame);
        return;
    }

    unsafe {
        let cr2: u64;
        asm!("mov {}, cr2", out(reg) cr2, options(nostack, preserves_flags));
        klog_debug!(
            "EXCEPTION: vec={} rip=0x{:x} err=0x{:x} cs=0x{:x} ss=0x{:x} cr2=0x{:x}",
            vector,
            frame_ref.rip,
            frame_ref.error_code,
            frame_ref.cs,
            frame_ref.ss,
            cr2
        );
    }

    if vector >= 32 {
        klog_info!("EXCEPTION: Unknown vector {}", vector);
        exception_default_panic(frame);
        return;
    }

    let critical = is_critical_exception_internal(vector);
    unsafe {
        if critical || !matches!(CURRENT_EXCEPTION_MODE, ExceptionMode::Test) {
            let name = cstr_to_str(get_exception_name(vector));
            klog_info!("EXCEPTION: Vector {} ({})", vector, name);
        }
    }

    let mut handler = unsafe { PANIC_HANDLERS[vector as usize] };
    if !critical && matches!(unsafe { CURRENT_EXCEPTION_MODE }, ExceptionMode::Test) {
        if let Some(override_handler) = unsafe { OVERRIDE_HANDLERS[vector as usize] } {
            handler = override_handler;
        }
    }

    handler(frame);
}
pub fn get_exception_name(vector: u8) -> *const c_char {
    match vector {
        0 => b"Divide Error\0".as_ptr() as *const c_char,
        1 => b"Debug\0".as_ptr() as *const c_char,
        2 => b"Non-Maskable Interrupt\0".as_ptr() as *const c_char,
        3 => b"Breakpoint\0".as_ptr() as *const c_char,
        4 => b"Overflow\0".as_ptr() as *const c_char,
        5 => b"Bound Range Exceeded\0".as_ptr() as *const c_char,
        6 => b"Invalid Opcode\0".as_ptr() as *const c_char,
        7 => b"Device Not Available\0".as_ptr() as *const c_char,
        8 => b"Double Fault\0".as_ptr() as *const c_char,
        10 => b"Invalid TSS\0".as_ptr() as *const c_char,
        11 => b"Segment Not Present\0".as_ptr() as *const c_char,
        12 => b"Stack Segment Fault\0".as_ptr() as *const c_char,
        13 => b"General Protection Fault\0".as_ptr() as *const c_char,
        14 => b"Page Fault\0".as_ptr() as *const c_char,
        16 => b"x87 FPU Error\0".as_ptr() as *const c_char,
        17 => b"Alignment Check\0".as_ptr() as *const c_char,
        18 => b"Machine Check\0".as_ptr() as *const c_char,
        19 => b"SIMD Floating-Point Exception\0".as_ptr() as *const c_char,
        _ => b"Unknown\0".as_ptr() as *const c_char,
    }
}

fn initialize_handler_tables() {
    unsafe {
        PANIC_HANDLERS = [exception_default_panic; 32];
        OVERRIDE_HANDLERS = [None; 32];

        PANIC_HANDLERS[EXCEPTION_DIVIDE_ERROR as usize] = exception_divide_error;
        PANIC_HANDLERS[EXCEPTION_DEBUG as usize] = exception_debug;
        PANIC_HANDLERS[EXCEPTION_NMI as usize] = exception_nmi;
        PANIC_HANDLERS[EXCEPTION_BREAKPOINT as usize] = exception_breakpoint;
        PANIC_HANDLERS[EXCEPTION_OVERFLOW as usize] = exception_overflow;
        PANIC_HANDLERS[EXCEPTION_BOUND_RANGE as usize] = exception_bound_range;
        PANIC_HANDLERS[EXCEPTION_INVALID_OPCODE as usize] = exception_invalid_opcode;
        PANIC_HANDLERS[EXCEPTION_DEVICE_NOT_AVAIL as usize] = exception_device_not_available;
        PANIC_HANDLERS[EXCEPTION_DOUBLE_FAULT as usize] = exception_double_fault;
        PANIC_HANDLERS[EXCEPTION_INVALID_TSS as usize] = exception_invalid_tss;
        PANIC_HANDLERS[EXCEPTION_SEGMENT_NOT_PRES as usize] = exception_segment_not_present;
        PANIC_HANDLERS[EXCEPTION_STACK_FAULT as usize] = exception_stack_fault;
        PANIC_HANDLERS[EXCEPTION_GENERAL_PROTECTION as usize] = exception_general_protection;
        PANIC_HANDLERS[EXCEPTION_PAGE_FAULT as usize] = exception_page_fault;
        PANIC_HANDLERS[EXCEPTION_FPU_ERROR as usize] = exception_fpu_error;
        PANIC_HANDLERS[EXCEPTION_ALIGNMENT_CHECK as usize] = exception_alignment_check;
        PANIC_HANDLERS[EXCEPTION_MACHINE_CHECK as usize] = exception_machine_check;
        PANIC_HANDLERS[EXCEPTION_SIMD_FP_EXCEPTION as usize] = exception_simd_fp_exception;
    }
}

fn is_critical_exception_internal(vector: u8) -> bool {
    matches!(
        vector,
        EXCEPTION_DOUBLE_FAULT | EXCEPTION_MACHINE_CHECK | EXCEPTION_NMI
    )
}

fn in_user(frame: &slopos_lib::InterruptFrame) -> bool {
    (frame.cs & 0x3) == 0x3
}

fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
    // SAFETY: All call sites provide statically defined, NUL-terminated byte
    // strings so this conversion cannot fail at runtime.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

fn terminate_user_task(
    reason: TaskFaultReason,
    frame: &slopos_lib::InterruptFrame,
    detail: &'static CStr,
) {
    let task = scheduler_get_current_task() as *mut Task;
    let tid = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };
    let detail_str = detail.to_str().unwrap_or("<invalid utf-8>");
    let (cr2, rip, rsp, vec, err) = unsafe {
        let cr2_val: u64;
        asm!("mov {}, cr2", out(reg) cr2_val, options(nomem, nostack, preserves_flags));
        (
            cr2_val,
            frame.rip,
            frame.rsp,
            frame.vector,
            frame.error_code,
        )
    };
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
            wl_award_loss();
            task_terminate(tid);
            scheduler_request_reschedule_from_interrupt();
        }
    }
    let _ = frame;
}

fn exception_default_panic(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Unhandled exception");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Unhandled exception\0".as_ptr() as *const c_char, frame);
}
pub fn exception_divide_error(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Divide by zero error");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Divide by zero error\0".as_ptr() as *const c_char, frame);
}
pub fn exception_debug(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("DEBUG: Debug exception occurred");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_nmi(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Non-maskable interrupt");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Non-maskable interrupt\0".as_ptr() as *const c_char, frame);
}
pub fn exception_breakpoint(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("DEBUG: Breakpoint exception");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_overflow(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Overflow exception");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_bound_range(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Bound range exceeded");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_invalid_opcode(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserUd,
            unsafe { &*frame },
            cstr_from_bytes(b"invalid opcode in user mode\0"),
        );
        return;
    }
    klog_info!("FATAL: Invalid opcode");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Invalid opcode\0".as_ptr() as *const c_char, frame);
}
pub fn exception_device_not_available(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserDeviceNa,
            unsafe { &*frame },
            cstr_from_bytes(b"device not available in user mode\0"),
        );
        return;
    }
    klog_info!("ERROR: Device not available");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_double_fault(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Double fault");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Double fault\0".as_ptr() as *const c_char, frame);
}
pub fn exception_invalid_tss(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Invalid TSS");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Invalid TSS\0".as_ptr() as *const c_char, frame);
}
pub fn exception_segment_not_present(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Segment not present");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Segment not present\0".as_ptr() as *const c_char, frame);
}
pub fn exception_stack_fault(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Stack segment fault");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Stack segment fault\0".as_ptr() as *const c_char, frame);
}
pub fn exception_general_protection(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TaskFaultReason::UserGp,
            unsafe { &*frame },
            cstr_from_bytes(b"general protection from user mode\0"),
        );
        return;
    }
    klog_info!("FATAL: General protection fault");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(
        b"General protection fault\0".as_ptr() as *const c_char,
        frame,
    );
}
pub fn exception_page_fault(frame: *mut slopos_lib::InterruptFrame) {
    let fault_addr: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) fault_addr, options(nomem, nostack, preserves_flags));
    }

    let mut stack_name: *const c_char = core::ptr::null();
    if safe_stack::safe_stack_guard_fault(fault_addr, &mut stack_name) != 0 {
        klog_info!("FATAL: Exception stack overflow detected via guard page");
        if !stack_name.is_null() {
            klog_info!("Guard page owner: {}", unsafe { cstr_to_str(stack_name) });
        }
        klog_info!("Fault address: 0x{:x}", fault_addr);
        kdiag_dump_interrupt_frame(frame);
        panic_with_frame(
            b"Exception stack overflow\0".as_ptr() as *const c_char,
            frame,
        );
        return;
    }

    let frame_ref = unsafe { &*frame };
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
        let mut pid = INVALID_TASK_ID;
        let mut cr3 = 0u64;
        let mut fault_phys = PhysAddr::NULL;
        let mut rsp_phys = PhysAddr::NULL;
        let mut rip_phys = PhysAddr::NULL;
        let mut ctx_rip = 0u64;
        let mut ctx_rsp = 0u64;
        let task_ptr = scheduler_get_current_task() as *mut Task;
        if !task_ptr.is_null() {
            pid = unsafe { (*task_ptr).process_id };
            unsafe {
                // TaskContext is packed, use addr_of! to get raw pointers
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
        terminate_user_task(
            TaskFaultReason::UserPage,
            unsafe { &*frame },
            cstr_from_bytes(b"user page fault\0"),
        );
        return;
    }

    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Page fault\0".as_ptr() as *const c_char, frame);
}
pub fn exception_fpu_error(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: x87 FPU error");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_alignment_check(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Alignment check");
    kdiag_dump_interrupt_frame(frame);
}
pub fn exception_machine_check(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Machine check");
    kdiag_dump_interrupt_frame(frame);
    panic_with_frame(b"Machine check\0".as_ptr() as *const c_char, frame);
}
pub fn exception_simd_fp_exception(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: SIMD floating-point exception");
    kdiag_dump_interrupt_frame(frame);
}

fn panic_with_frame(message: *const c_char, frame: *mut slopos_lib::InterruptFrame) {
    let frame_ref = unsafe { &*frame };
    kernel_panic_with_state(message, frame_ref.rip, frame_ref.rsp);
}
