#![allow(bad_asm_style)]

use core::arch::{asm, global_asm};
use core::cell::SyncUnsafeCell;
use core::ffi::c_void;

use slopos_arch::cpu;
use slopos_utils::{klog_debug, klog_info};

use crate::exception::*;
use crate::ist_stacks;
use crate::user_fault::*;

global_asm!(include_str!("../idt_handlers.s"));

pub use slopos_arch::arch::idt::{
    EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_SEGMENT_NOT_PRES, EXCEPTION_SIMD_FP_EXCEPTION,
    EXCEPTION_STACK_FAULT, IDT_ENTRIES, IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IRQ_BASE_VECTOR,
    IdtEntry, LAPIC_TIMER_VECTOR, LUF_DRAIN_IPI_VECTOR, MSI_VECTOR_BASE, MSI_VECTOR_COUNT,
    RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR, SYSCALL_VECTOR, TLB_SHOOTDOWN_VECTOR,
};

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

type ExceptionHandler = fn(*mut slopos_arch::InterruptFrame);

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

use slopos_core::irq::irq_dispatch;
use slopos_core::syscall::syscall_handle;
use slopos_drivers::apic::send_eoi;
use slopos_mm::tlb;

use slopos_core::sched::{
    RescheduleReason, TrapExitSource, scheduler_handoff_on_trap_exit, scheduler_request_reschedule,
};

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
    fn isr_rcu_qs_ipi();
    fn isr_luf_drain_ipi();
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
        RCU_QS_IPI_VECTOR,
        handler_ptr(isr_rcu_qs_ipi),
        0x08,
        IDT_GATE_INTERRUPT,
    );
    idt_set_gate(
        LUF_DRAIN_IPI_VECTOR,
        handler_ptr(isr_luf_drain_ipi),
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
    slopos_arch::arch::exception::exception_is_critical(vector) as i32
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
    if let Some(cpu_idx) = slopos_arch::pcr::cpu_index_from_apic_id(apic_id) {
        tlb::handle_shootdown_ipi(cpu_idx);
    } else {
        klog_debug!(
            "TLB: Missing CPU index for APIC 0x{:x}; cannot ack shootdown",
            apic_id
        );
    }
    send_eoi();
}

fn handle_luf_drain_ipi() {
    let apic_id = slopos_drivers::apic::get_id();
    if let Some(cpu_idx) = slopos_arch::pcr::cpu_index_from_apic_id(apic_id) {
        slopos_mm::mmu::luf::handle_drain_ipi(cpu_idx);
    } else {
        klog_debug!(
            "LUF: Missing CPU index for APIC 0x{:x}; cannot ack drain",
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
                slopos_arch::pcr::current_pcr()
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
                slopos_arch::pcr::current_pcr()
                    .preempt_count
                    .fetch_sub(1, core::sync::atomic::Ordering::Release);
            }
        }
    }
}

/// NMI watchdog handler -- invoked when a neighbouring CPU sends an NMI
/// because this CPU has not recorded a timer tick for >500 ms.
///
/// Dumps the faulting context, reports held-lock state, force-unlocks all
/// tracked locks (so other CPUs are not permanently blocked), and panics.
fn nmi_watchdog_handler(frame: &slopos_arch::InterruptFrame) {
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Use serial logging directly to avoid lock recursion.
    klog_info!(
        "NMI WATCHDOG: CPU {} locked up! RIP={:#x} RSP={:#x} CS={:#x}",
        cpu_id,
        frame.rip,
        frame.rsp,
        frame.cs
    );

    let held = slopos_sync::held_lock_count();
    klog_info!("NMI WATCHDOG: CPU {} holds {} lock(s)", cpu_id, held);

    // Force-release all tracked locks so other CPUs can make progress.
    unsafe {
        slopos_sync::poison_unlock_all_held();
    }

    panic!("NMI WATCHDOG: CPU {} not responding for >500ms", cpu_id);
}

/// Implementation of common_exception_handler - called from FFI boundary
pub fn common_exception_handler_impl(frame: *mut slopos_arch::InterruptFrame) {
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

    // NMI watchdog: vector 2 is used by the cross-CPU deadlock detector.
    // Handle it before any other dispatch to keep the path minimal.
    if vector == EXCEPTION_NMI {
        nmi_watchdog_handler(frame_ref);
        return;
    }

    if vector == SYSCALL_VECTOR {
        syscall_handle(frame);
        return;
    }

    if vector == TLB_SHOOTDOWN_VECTOR {
        handle_tlb_shootdown_ipi();
        return;
    }

    if vector == LUF_DRAIN_IPI_VECTOR {
        handle_luf_drain_ipi();
        return;
    }

    if vector == RCU_QS_IPI_VECTOR {
        slopos_sync::rcu_note_qs();
        send_eoi();
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
        // Snapshot ALL IRET payload fields BEFORE the handler runs.
        // After the handler + scheduler, compare to detect silent corruption
        // of any field (not just CS/SS).
        let pre_rip = frame_ref.rip;
        let pre_cs = frame_ref.cs;
        let pre_rflags = frame_ref.rflags;
        let pre_rsp = frame_ref.rsp;
        let pre_ss = frame_ref.ss;

        slopos_core::irq::increment_timer_ticks();
        // EOI before handler: prevents timer starvation if the handler is
        // slow (e.g. blocked on a lock). Safe because the interrupt gate
        // clears IF, so the next timer interrupt won't nest — it stays
        // pending until IRET re-enables interrupts.
        send_eoi();
        slopos_core::sched::scheduler_handle_timer_interrupt(frame);
        scheduler_handoff_on_trap_exit(TrapExitSource::Irq);

        // Re-read the frame from the stack pointer — if context_switch saved
        // and restored our context, `frame` still points at the same stack
        // address and the CPU-pushed fields (rip/cs/rflags/rsp/ss) must be
        // untouched.  Corruption here means something overwrote the ISR's
        // interrupt frame while we were switched out.
        let post_rip = unsafe { core::ptr::read_volatile(&(*frame).rip) };
        let post_cs = unsafe { core::ptr::read_volatile(&(*frame).cs) };
        let post_rflags = unsafe { core::ptr::read_volatile(&(*frame).rflags) };
        let post_rsp = unsafe { core::ptr::read_volatile(&(*frame).rsp) };
        let post_ss = unsafe { core::ptr::read_volatile(&(*frame).ss) };

        if post_rip != pre_rip
            || post_cs != pre_cs
            || post_rflags != pre_rflags
            || post_rsp != pre_rsp
            || post_ss != pre_ss
        {
            klog_info!(
                "TIMER IRET CORRUPTION: rip 0x{:x}->0x{:x} cs 0x{:x}->0x{:x} rflags 0x{:x}->0x{:x} rsp 0x{:x}->0x{:x} ss 0x{:x}->0x{:x} frame={:p}",
                pre_rip,
                post_rip,
                pre_cs,
                post_cs,
                pre_rflags,
                post_rflags,
                pre_rsp,
                post_rsp,
                pre_ss,
                post_ss,
                frame
            );
            // Do not resume with a corrupted IRET frame — that would
            // fault or silently execute in the wrong context.
            panic!("TIMER IRET frame corruption detected");
        }
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
            let name = slopos_arch::arch::exception::get_exception_name(vector);
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
        // NMI (vector 2) is handled directly in common_exception_handler_impl
        // by the watchdog handler -- no table entry needed.
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
/// Called when the ISR's pre-IRETQ CS validation detects a corrupt IRET
/// frame.  Logs the corruption for debugging, then panics.
///
/// # Safety
/// `iret_frame` must point to a readable region of at least 5 consecutive
/// `u64` values laid out as `[RIP, CS, RFLAGS, RSP, SS]`.  The pointer
/// need not be aligned (values are read with `read_unaligned`).
pub(crate) unsafe fn handle_corrupt_iret_frame(iret_frame: *const u64) -> ! {
    use slopos_utils::klog_info;

    // SAFETY: caller guarantees iret_frame points to 5 readable u64s.
    let (rip, cs, rflags, rsp, ss) = unsafe {
        (
            core::ptr::read_unaligned(iret_frame),
            core::ptr::read_unaligned(iret_frame.add(1)),
            core::ptr::read_unaligned(iret_frame.add(2)),
            core::ptr::read_unaligned(iret_frame.add(3)),
            core::ptr::read_unaligned(iret_frame.add(4)),
        )
    };

    klog_info!(
        "ISR IRET FRAME CORRUPT: CS=0x{:x} (expected 0x08 or 0x23)",
        cs
    );
    klog_info!(
        "  RIP=0x{:x} RFLAGS=0x{:x} RSP=0x{:x} SS=0x{:x} frame_ptr={:p}",
        rip,
        rflags,
        rsp,
        ss,
        iret_frame
    );

    // Dump the surrounding stack words for forensics.
    // Limit the probe range to avoid nested faults on invalid addresses:
    // use the current task's kernel stack bounds when available, otherwise
    // conservatively bound to ±128 bytes around iret_frame.
    // Call scheduler_get_current_task() once and reuse for both the
    // bounds computation and the task-info dump below.
    let task_ptr = slopos_core::sched::scheduler_get_current_task();

    klog_info!("=== IRET FRAME VICINITY DUMP ===");
    let (dump_lo, dump_hi) = {
        if !task_ptr.is_null() {
            let base = unsafe { (*task_ptr).kernel_stack_base } as usize;
            let top = unsafe { (*task_ptr).kernel_stack_top } as usize;
            if base != 0 && top > base {
                (base, top)
            } else {
                let mid = iret_frame as usize;
                (mid.saturating_sub(128), mid.saturating_add(128))
            }
        } else {
            let mid = iret_frame as usize;
            (mid.saturating_sub(128), mid.saturating_add(128))
        }
    };
    for offset in -4isize..10 {
        // Use wrapping_offset to avoid UB when the resulting pointer
        // falls outside the allocated object (negative offsets).
        let addr = iret_frame.wrapping_offset(offset);
        let addr_val = addr as usize;
        if addr_val < dump_lo || addr_val + 8 > dump_hi {
            klog_info!("  [{:+}] {:p} = <out of bounds>", offset, addr);
            continue;
        }
        let val = unsafe { core::ptr::read_unaligned(addr) };
        let marker = if offset == 0 {
            " <-- RIP"
        } else if offset == 1 {
            " <-- CS (BAD)"
        } else if offset == 2 {
            " <-- RFLAGS"
        } else if offset == 3 {
            " <-- RSP"
        } else if offset == 4 {
            " <-- SS"
        } else {
            ""
        };
        klog_info!("  [{:+}] {:p} = 0x{:016x}{}", offset, addr, val, marker);
    }
    klog_info!("=== END DUMP ===");

    if !task_ptr.is_null() {
        let is_user = unsafe { (*task_ptr).flags } & slopos_abi::task::TASK_FLAG_USER_MODE != 0;
        klog_info!(
            "  Current task: id={} user={}",
            unsafe { (*task_ptr).task_id },
            is_user,
        );
    }

    // User-mode IRET corruption is fatal — a corrupted frame cannot be
    // recovered and the panic below fires regardless.
    panic!("Unrecoverable IRET frame corruption");
}

fn try_handle_page_fault(frame: *mut slopos_arch::InterruptFrame) -> bool {
    let fault_addr = cpu::read_cr2();
    let frame_ref = unsafe { &*frame };

    if ist_stacks::ist_guard_fault(fault_addr, core::ptr::null_mut()) != 0 {
        return false;
    }

    // Kernel-mode fault inside the usercopy assembly region: recover
    // gracefully by redirecting RIP to the fault return label, which
    // returns a nonzero "remaining bytes" value to the Rust caller.
    // This makes copy_from_user / copy_to_user safe against concurrent
    // munmap on SMP (Redox OS pattern).
    if !in_user(frame_ref) {
        if slopos_mm::user_copy::is_usercopy_ip(frame_ref.rip) {
            let frame_mut = unsafe { &mut *frame };
            frame_mut.rip = slopos_mm::user_copy::usercopy_fault_ip();
            return true;
        }
        return false;
    }
    let task_ptr = resolve_user_fault_task();
    if task_ptr.is_null() {
        return false;
    }
    let pid = unsafe { (*task_ptr).process_id };
    let tid = unsafe { (*task_ptr).task_id };

    slopos_mm::page_fault::try_resolve_user_fault(fault_addr, frame_ref.error_code, pid, tid)
}
