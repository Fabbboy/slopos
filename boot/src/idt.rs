#![allow(static_mut_refs)]
#![allow(bad_asm_style)]

use core::arch::{asm, global_asm};
use core::ffi::{c_char, CStr};

use slopos_drivers::serial_println;
use slopos_lib::{klog_debug, klog_info};

use crate::kernel_panic::kernel_panic;
use crate::safe_stack;

global_asm!(include_str!("../idt_handlers.s"));

pub const IDT_GATE_INTERRUPT: u8 = 0x8E;
pub const IDT_GATE_TRAP: u8 = 0x8F;

pub const EXCEPTION_DIVIDE_ERROR: u8 = 0;
pub const EXCEPTION_DEBUG: u8 = 1;
pub const EXCEPTION_NMI: u8 = 2;
pub const EXCEPTION_BREAKPOINT: u8 = 3;
pub const EXCEPTION_OVERFLOW: u8 = 4;
pub const EXCEPTION_BOUND_RANGE: u8 = 5;
pub const EXCEPTION_INVALID_OPCODE: u8 = 6;
pub const EXCEPTION_DEVICE_NOT_AVAIL: u8 = 7;
pub const EXCEPTION_DOUBLE_FAULT: u8 = 8;
pub const EXCEPTION_INVALID_TSS: u8 = 10;
pub const EXCEPTION_SEGMENT_NOT_PRES: u8 = 11;
pub const EXCEPTION_STACK_FAULT: u8 = 12;
pub const EXCEPTION_GENERAL_PROTECTION: u8 = 13;
pub const EXCEPTION_PAGE_FAULT: u8 = 14;
pub const EXCEPTION_FPU_ERROR: u8 = 16;
pub const EXCEPTION_ALIGNMENT_CHECK: u8 = 17;
pub const EXCEPTION_MACHINE_CHECK: u8 = 18;
pub const EXCEPTION_SIMD_FP_EXCEPTION: u8 = 19;

pub const IRQ_BASE_VECTOR: u8 = 32;
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const IDT_ENTRIES: usize = 256;

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

type ExceptionHandler = extern "C" fn(*mut slopos_lib::InterruptFrame);

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
#[allow(dead_code)]
struct Idtr {
    limit: u16,
    base: u64,
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum ExceptionMode {
    Normal = 0,
    Test = 1,
}

unsafe extern "C" {
    fn irq_dispatch(frame: *mut slopos_lib::InterruptFrame);
    fn syscall_handle(frame: *mut slopos_lib::InterruptFrame);
    fn scheduler_request_reschedule_from_interrupt();
    fn scheduler_get_current_task() -> *mut Task;
    fn task_terminate(task_id: u32) -> i32;
    fn kdiag_dump_interrupt_frame(frame: *const slopos_lib::InterruptFrame);
    fn wl_award_loss();
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; 32],
    pub state: u8,
    pub priority: u8,
    pub flags: u16,
    pub process_id: u32,
    pub stack_base: u64,
    pub stack_size: u64,
    pub stack_pointer: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,
    pub entry_point: u64,
    pub entry_arg: u64,
    pub context: [u8; 0xC8], // padding for context structure
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub waiting_on_task_id: u32,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: u16,
    pub fault_reason: u16,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
}

const TASK_EXIT_REASON_USER_FAULT: u16 = 2;
const TASK_FAULT_USER_PAGE: u16 = 1;
const TASK_FAULT_USER_GP: u16 = 2;
const TASK_FAULT_USER_UD: u16 = 3;
const TASK_FAULT_USER_DEVICE_NA: u16 = 4;
const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;

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

#[unsafe(no_mangle)]
pub extern "C" fn idt_init() {
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

#[unsafe(no_mangle)]
pub extern "C" fn idt_set_gate_priv(vector: u8, handler: u64, selector: u16, typ: u8, dpl: u8) {
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

#[unsafe(no_mangle)]
pub extern "C" fn idt_set_gate(vector: u8, handler: u64, selector: u16, typ: u8) {
    idt_set_gate_priv(vector, handler, selector, typ, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn idt_get_gate(vector: u8, out_entry: *mut IdtEntry) -> i32 {
    if out_entry.is_null() || vector as usize >= IDT_ENTRIES {
        return -1;
    }
    unsafe {
        *out_entry = IDT[vector as usize];
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn idt_install_exception_handler(vector: u8, handler: ExceptionHandler) {
    if vector >= 32 {
        klog_info!(
            "IDT: Ignoring handler install for non-exception vector {}",
            vector
        );
        return;
    }
    if is_critical_exception_internal(vector) {
        klog_info!(
            "IDT: Refusing to override critical exception {}",
            vector
        );
        return;
    }
    unsafe {
        OVERRIDE_HANDLERS[vector as usize] = Some(handler);
        klog_debug!(
            "IDT: Registered override handler for exception {}",
            vector
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn idt_set_ist(vector: u8, ist_index: u8) {
    if vector as usize >= IDT_ENTRIES {
        klog_info!(
            "IDT: Invalid IST assignment for vector {}",
            vector
        );
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

#[unsafe(no_mangle)]
pub extern "C" fn exception_set_mode(mode: ExceptionMode) {
    unsafe {
        CURRENT_EXCEPTION_MODE = mode;
        if let ExceptionMode::Normal = mode {
            OVERRIDE_HANDLERS = [None; 32];
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_is_critical(vector: u8) -> i32 {
    is_critical_exception_internal(vector) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn idt_load() {
    unsafe {
        IDT_POINTER.limit = (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        IDT_POINTER.base = IDT.as_ptr() as u64;
        let idtr = &raw const IDT_POINTER;
        asm!("lidt [{}]", in(reg) idtr, options(nostack, preserves_flags));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn common_exception_handler(frame: *mut slopos_lib::InterruptFrame) {
    let frame_ref = unsafe { &mut *frame };
    let vector = (frame_ref.vector & 0xFF) as u8;

    safe_stack::safe_stack_record_usage(vector, frame as u64);

    if vector == SYSCALL_VECTOR {
        unsafe { syscall_handle(frame) };
        return;
    }

    if vector >= IRQ_BASE_VECTOR {
        unsafe { irq_dispatch(frame) };
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
            let name = CStr::from_ptr(get_exception_name(vector))
                .to_str()
                .unwrap_or("Unknown");
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

#[unsafe(no_mangle)]
pub extern "C" fn get_exception_name(vector: u8) -> *const c_char {
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

fn terminate_user_task(reason: u16, frame: &slopos_lib::InterruptFrame, detail: &'static CStr) {
    let task = unsafe { scheduler_get_current_task() };
    let tid = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };
    let detail_str = detail.to_str().unwrap_or("<invalid utf-8>");
    klog_info!("Terminating user task {}: {}", tid, detail_str);
    if !task.is_null() {
        unsafe {
            (*task).exit_reason = TASK_EXIT_REASON_USER_FAULT;
            (*task).fault_reason = reason;
            (*task).exit_code = 1;
            wl_award_loss();
            task_terminate(tid);
            scheduler_request_reschedule_from_interrupt();
        }
    }
    let _ = frame;
}

extern "C" fn exception_default_panic(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Unhandled exception");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Unhandled exception\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_divide_error(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Divide by zero error");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Divide by zero error\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_debug(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("DEBUG: Debug exception occurred");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_nmi(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Non-maskable interrupt");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Non-maskable interrupt\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_breakpoint(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("DEBUG: Breakpoint exception");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_overflow(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Overflow exception");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_bound_range(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Bound range exceeded");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_invalid_opcode(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TASK_FAULT_USER_UD,
            unsafe { &*frame },
            cstr_from_bytes(b"invalid opcode in user mode\0"),
        );
        return;
    }
    klog_info!("FATAL: Invalid opcode");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Invalid opcode\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_device_not_available(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TASK_FAULT_USER_DEVICE_NA,
            unsafe { &*frame },
            cstr_from_bytes(b"device not available in user mode\0"),
        );
        return;
    }
    klog_info!("ERROR: Device not available");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_double_fault(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Double fault");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Double fault\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_invalid_tss(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Invalid TSS");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Invalid TSS\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_segment_not_present(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Segment not present");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Segment not present\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_stack_fault(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Stack segment fault");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Stack segment fault\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_general_protection(frame: *mut slopos_lib::InterruptFrame) {
    if in_user(unsafe { &*frame }) {
        terminate_user_task(
            TASK_FAULT_USER_GP,
            unsafe { &*frame },
            cstr_from_bytes(b"general protection from user mode\0"),
        );
        return;
    }
    klog_info!("FATAL: General protection fault");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
        kernel_panic(b"General protection fault\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_page_fault(frame: *mut slopos_lib::InterruptFrame) {
    let fault_addr: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) fault_addr, options(nomem, nostack, preserves_flags));
    }

    let mut stack_name: *const c_char = core::ptr::null();
    if safe_stack::safe_stack_guard_fault(fault_addr, &mut stack_name) != 0 {
        klog_info!("FATAL: Exception stack overflow detected via guard page");
        if !stack_name.is_null() {
            let owner = unsafe { CStr::from_ptr(stack_name) }
                .to_str()
                .unwrap_or("<invalid utf-8>");
            klog_info!("Guard page owner: {}", owner);
        }
        klog_info!("Fault address: 0x{:x}", fault_addr);
        unsafe {
            kdiag_dump_interrupt_frame(frame);
        }
        kernel_panic(b"Exception stack overflow\0".as_ptr() as *const c_char);
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
        terminate_user_task(
            TASK_FAULT_USER_PAGE,
            unsafe { &*frame },
            cstr_from_bytes(b"user page fault\0"),
        );
        return;
    }

    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Page fault\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_fpu_error(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: x87 FPU error");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_alignment_check(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: Alignment check");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_machine_check(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("FATAL: Machine check");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
    kernel_panic(b"Machine check\0".as_ptr() as *const c_char);
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_simd_fp_exception(frame: *mut slopos_lib::InterruptFrame) {
    klog_info!("ERROR: SIMD floating-point exception");
    unsafe {
        kdiag_dump_interrupt_frame(frame);
    }
}
