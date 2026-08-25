#![allow(bad_asm_style)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU8, Ordering};

use slopos_arch::cpu;
pub use slopos_ostd::irq::{
    EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_SEGMENT_NOT_PRES, EXCEPTION_SIMD_FP_EXCEPTION,
    EXCEPTION_STACK_FAULT, IRQ_BASE_VECTOR, IdtBuilder, IdtEntry, IstPreemptHold,
    LAPIC_TIMER_VECTOR, RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR, SHUTDOWN_VECTOR, SYSCALL_VECTOR,
    TLB_SHOOTDOWN_VECTOR,
};
use slopos_ostd::{kdiag_dump_interrupt_frame, klog_debug, klog_info};

use crate::exception::*;
use crate::ist_stacks;
use crate::user_fault::*;

const _: () = {
    use core::mem::offset_of;
    // Must match the asm unwind in slopos-ostd/src/irq/asm/handlers.s.
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rip) == 136);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, cs) == 144);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rflags) == 152);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rsp) == 160);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, ss) == 168);
    // OSTD's `__ostd_user_return` trampoline reads this slot as `gs:[16]`.
    assert!(slopos_ostd::cpu::x86_64::pcr::offsets::KERNEL_RSP == 16);
};

static BUILDER: IdtBuilder = IdtBuilder::new();

type ExceptionHandler = fn(*mut slopos_arch::InterruptFrame);

static CURRENT_EXCEPTION_MODE: AtomicU8 = AtomicU8::new(ExceptionMode::Normal as u8);

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum ExceptionMode {
    Normal = 0,
    Test = 1,
}

impl ExceptionMode {
    fn load() -> Self {
        match CURRENT_EXCEPTION_MODE.load(Ordering::Acquire) {
            x if x == ExceptionMode::Test as u8 => ExceptionMode::Test,
            _ => ExceptionMode::Normal,
        }
    }

    fn store(self) {
        CURRENT_EXCEPTION_MODE.store(self as u8, Ordering::Release);
    }
}

/// Per-vector handler tables; null encodes "no handler installed".
///
/// Per-slot atomic stores keep the 256-byte fn-pointer array off the stack,
/// which the kernel's stack-frame budget would otherwise reject.
mod handler_tables {
    use super::ExceptionHandler;
    use core::sync::atomic::{AtomicPtr, Ordering};
    use slopos_ostd::arch::x86_64::kernel_ptr::fn_ptr;

    const NULL_SLOT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
    static PANIC: [AtomicPtr<()>; 32] = [NULL_SLOT; 32];
    static OVERRIDE: [AtomicPtr<()>; 32] = [NULL_SLOT; 32];

    pub fn install_panic(vector: u8, handler: ExceptionHandler) {
        PANIC[vector as usize].store(
            fn_ptr::encode::<ExceptionHandler>(Some(handler)),
            Ordering::Release,
        );
    }

    pub fn panic_for(vector: u8) -> Option<ExceptionHandler> {
        fn_ptr::decode::<ExceptionHandler>(PANIC[vector as usize].load(Ordering::Acquire))
    }

    pub fn install_override(vector: u8, handler: Option<ExceptionHandler>) {
        OVERRIDE[vector as usize].store(
            fn_ptr::encode::<ExceptionHandler>(handler),
            Ordering::Release,
        );
    }

    pub fn override_for(vector: u8) -> Option<ExceptionHandler> {
        fn_ptr::decode::<ExceptionHandler>(OVERRIDE[vector as usize].load(Ordering::Acquire))
    }

    pub fn clear_overrides() {
        for slot in OVERRIDE.iter() {
            slot.store(core::ptr::null_mut(), Ordering::Release);
        }
    }
}

fn panic_handler_for(vector: u8) -> ExceptionHandler {
    handler_tables::panic_for(vector).unwrap_or(exception_default_panic)
}

use slopos_drivers::apic::send_eoi;
use slopos_mm::tlb;

use slopos_sched::scheduler::{
    RescheduleReason, TrapExitSource, scheduler_handoff_on_trap_exit, scheduler_request_reschedule,
};

struct IrqNestHold {
    active: bool,
}

impl IrqNestHold {
    fn enter() -> Self {
        slopos_ostd::cpu::x86_64::pcr::interrupt_nesting_enter();
        Self { active: true }
    }

    fn reenter(&mut self) {
        if !self.active {
            slopos_ostd::cpu::x86_64::pcr::interrupt_nesting_enter();
            self.active = true;
        }
    }

    fn leave(&mut self) {
        if self.active {
            slopos_ostd::cpu::x86_64::pcr::interrupt_nesting_exit();
            self.active = false;
        }
    }

    /// Runs outside interrupt-nesting context so a switched-in task is not
    /// observed as in-interrupt. The scheduler prologue sits beneath a
    /// non-unwindable asm trampoline, so a panic there must abort.
    fn handoff(&mut self, source: TrapExitSource) {
        self.leave();
        let abort = slopos_ostd::panic::AbortOnUnwind::new();
        scheduler_handoff_on_trap_exit(source);
        abort.disarm();
        self.reenter();
    }
}

impl Drop for IrqNestHold {
    fn drop(&mut self) {
        self.leave();
    }
}

/// One-shot BSP-only IDT initialisation; the `BspToken` witness binds the call
/// to the BSP-init scope opened by `slopos_ostd::sync::run_bsp_init`.
pub fn idt_init<'b>(_token: &slopos_ostd::sync::BspToken<'b>) {
    klog_debug!("IDT: init start");
    BUILDER.install_default_handlers();
    initialize_handler_tables();
    klog_debug!("IDT: install_default_handlers + handler tables ready");
}

pub fn idt_set_gate_priv(vector: u8, handler: u64, selector: u16, typ: u8, dpl: u8) {
    BUILDER.set_gate_priv(vector, handler, selector, typ, dpl);
}

pub fn idt_set_gate(vector: u8, handler: u64, selector: u16, typ: u8) {
    BUILDER.set_gate(vector, handler, selector, typ);
}

pub fn idt_get_gate(vector: u8, out_entry: *mut IdtEntry) -> i32 {
    BUILDER.write_gate_to_caller(vector, out_entry)
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
    handler_tables::install_override(vector, Some(handler));
    klog_debug!("IDT: Registered override handler for exception {}", vector);
}

/// Bind an IDT entry to an IST slot. `&mut BootCtx<'_, K>` gates the call so
/// post-boot code cannot rebind interrupt stacks; `K: CpuInitKind` keeps it
/// callable from BSP-init, AP-init and test scopes alike.
pub fn idt_set_ist<'b, K: slopos_hermetic::CpuInitKind>(
    _ctx: &mut slopos_hermetic::BootCtx<'b, K>,
    vector: u8,
    slot: slopos_arch::arch::gdt::IstSlot,
) {
    BUILDER.set_ist(vector, slot.as_index() & 0x7);
}

pub fn exception_set_mode(mode: ExceptionMode) {
    mode.store();
    if let ExceptionMode::Normal = mode {
        handler_tables::clear_overrides();
    }
}

pub fn exception_is_critical(vector: u8) -> i32 {
    slopos_arch::arch::exception::exception_is_critical(vector) as i32
}

/// Load the static IDT on the current CPU. Any `CpuInitWitness` is accepted:
/// the witness gates the call to a boot-init scope without distinguishing BSP
/// from AP, and both bringup paths call this.
pub fn idt_load<W: slopos_ostd::sync::CpuInitWitness>(witness: &W) {
    BUILDER.load_static(witness);
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

/// Answer an NMI: the lockup detector's probe, the TLB ladder's, or one
/// nobody armed.
///
/// A *returning* NMI may take no lock, so output goes through the watchdog's
/// byte-at-a-time emitter. [`nmi_die`] is exempt: nothing resumes there.
fn nmi_handler(frame: &slopos_arch::InterruptFrame) {
    use slopos_ostd::watchdog::{self, NmiDisposition};

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // A peer CPU winning the fatal-panic election makes this NMI its stop
    // broadcast, not a watchdog wedge: stop silently so the owner drives the
    // console alone. A self-directed NMI on the owner must not self-stop.
    if slopos_ostd::panic::panic_owner_claimed()
        && !slopos_ostd::panic::panic_owner_is(cpu_id as u32)
    {
        // We are about to halt, so a non-panicking initiator would spin forever
        // on an ack we will never deliver. Set-only — never clears an ack.
        slopos_mm::tlb::force_ack_local_shootdowns(cpu_id);
        slopos_mm::tlb::notify_cpu_offline();
        slopos_arch::pcr::mark_cpu_offline(cpu_id);
        slopos_sched::per_cpu::abandon_dispatch_for_dying_cpu(cpu_id);
        slopos_ostd::panic::mark_fatal_abort();
        slopos_ostd::sync::panic_recovery::poison_all_held_locks_no_halt();
        slopos_ostd::panic::mark_cpu_stopped();
        slopos_arch::cpu::disable_interrupts();
        slopos_arch::cpu::halt_loop();
    }

    // Registers only: the wait-for chain is the watcher's to print, and it runs
    // whether or not this NMI is ever delivered.
    let disposition = watchdog::probe_disposition(cpu_id);
    nmi_emit_context(cpu_id, frame, disposition);

    if disposition == NmiDisposition::Fatal {
        nmi_die(cpu_id, frame);
    }

    // Neither an unsolicited NMI nor an operator probe is evidence of a fault
    // here, so neither may spend the budget `panic.oops_limit=` bounds.
    if disposition != NmiDisposition::Unsolicited && disposition != NmiDisposition::Probe {
        let (count, _limit_reached) = slopos_ostd::panic_recovery::oops_record();
        watchdog::nmi_emit("NMI: oops ");
        watchdog::nmi_emit_dec(count);
        watchdog::nmi_emit_line(" recorded, resuming");
    }

    // Must be last: the detector will not re-send until this runs.
    watchdog::release_probe(cpu_id);
}

/// Format-free and fault-free: a returning NMI must take no page fault, because
/// that fault's own `iretq` would unblock NMI while this handler still runs.
fn nmi_emit_context(
    cpu_id: usize,
    frame: &slopos_arch::InterruptFrame,
    disposition: slopos_ostd::watchdog::NmiDisposition,
) {
    use slopos_ostd::watchdog::{
        NmiDisposition, nmi_emit, nmi_emit_dec, nmi_emit_hex, nmi_emit_line,
    };

    // Everything below goes to the UART, which a machine may not have.
    slopos_ostd::watchdog::note_probe_rip(cpu_id, frame.rip);

    nmi_emit("NMI: cpu ");
    nmi_emit_dec(cpu_id as u64);
    nmi_emit(match disposition {
        NmiDisposition::Report => " stalled",
        NmiDisposition::Fatal => " stalled (fatal)",
        NmiDisposition::TlbLadder => " never acked a TLB shootdown",
        NmiDisposition::Probe => " probed",
        NmiDisposition::Unsolicited => " took an unsolicited NMI",
    });
    nmi_emit(" rip=");
    nmi_emit_hex(frame.rip);
    // Binary search over a `'static` rodata array: no lock, no allocation, and
    // no dereference of anything the fault could have corrupted.
    if let Some(sym) = slopos_ostd::ksym::lookup(frame.rip) {
        nmi_emit(" <");
        nmi_emit(sym.symbol);
        nmi_emit("+0x");
        nmi_emit_hex(sym.offset);
        nmi_emit(">");
    }
    nmi_emit(" rsp=");
    nmi_emit_hex(frame.rsp);
    nmi_emit(" rbp=");
    nmi_emit_hex(frame.rbp);
    nmi_emit(" cs=");
    nmi_emit_hex(frame.cs);
    nmi_emit_line("");
}

/// Terminal branch: everything destructive lives here and nowhere else, because
/// force-releasing locks and latching the validator off are corrections on the
/// way out rather than diagnostics.
fn nmi_die(cpu_id: usize, frame: &slopos_arch::InterruptFrame) -> ! {
    use slopos_ostd::watchdog::{nmi_emit, nmi_emit_dec, nmi_emit_hex, nmi_emit_line};

    // Ahead of the backtrace walk below, which can fault and never return.
    // `force_ack` is not redundant with `notify_cpu_offline`: that one only
    // removes this CPU from future target selection, leaving a shootdown
    // already in flight to burn its re-sends into a panic.
    slopos_mm::tlb::force_ack_local_shootdowns(cpu_id);
    slopos_mm::tlb::notify_cpu_offline();
    slopos_arch::pcr::mark_cpu_offline(cpu_id);
    slopos_sched::per_cpu::abandon_dispatch_for_dying_cpu(cpu_id);

    // Each frame is [saved_rbp][return_addr]. The per-read validation cannot
    // prove rbp lies inside the interrupted task's stack, so a fault can still
    // slip through — the walk runs only here, where we are already dying.
    {
        use slopos_ostd::arch::x86_64::kernel_ptr::read_volatile_canonical_kernel_u64;
        let mut rbp = frame.rbp;
        nmi_emit("NMI: cpu ");
        nmi_emit_dec(cpu_id as u64);
        nmi_emit_line(" backtrace:");
        nmi_emit("  ");
        nmi_emit_hex(frame.rip);
        nmi_emit_line("");
        for _ in 1..=16u32 {
            let Some(next_rbp) = read_volatile_canonical_kernel_u64(rbp) else {
                break;
            };
            let Some(ret_addr) = read_volatile_canonical_kernel_u64(rbp.wrapping_add(8)) else {
                break;
            };
            nmi_emit("  ");
            nmi_emit_hex(ret_addr);
            nmi_emit_line("");
            if next_rbp <= rbp {
                break;
            }
            rbp = next_rbp;
        }
    }

    // Puts the wedge site on the panic screen, not just the serial log.
    crate::panic::set_panic_cpu_state(frame.rip, frame.rsp, frame.rbp);
    slopos_ostd::sync::panic_recovery::poison_all_held_locks_no_halt();

    // Not `panic!`: the panic strategy is `unwind`, and the interrupt-entry
    // asm frame below carries no unwind information.
    crate::panic::panic_abort_raw("NMI watchdog: CPU made no progress, sustained")
}

/// `int 0x80` is not a SlopOS syscall ABI — userland enters through `SYSCALL` /
/// `LSTAR`. The gate stays user-reachable so the trap is answered rather than
/// escalating.
fn answer_legacy_syscall(frame_ref: &mut slopos_arch::InterruptFrame) {
    frame_ref.rax = slopos_abi::syscall::ENOSYS_RETURN;
}

/// Called from the `common_exception_handler` FFI boundary.
pub fn common_exception_handler_impl(frame: *mut slopos_arch::InterruptFrame) {
    // The frame lives for exactly this invocation, so a frame-local is the
    // honest anchor for a borrow of it.
    let mut frame_anchor = ();
    let frame_ref = slopos_arch::InterruptFrame::from_ptr_mut(&mut frame_anchor, frame)
        .expect("common_exception_handler_impl: null frame ptr");
    let vector = (frame_ref.vector & 0xFF) as u8;

    // IST stacks are per-vector, per-CPU fixed addresses, and every exception
    // (vectors 0-31) uses one. Switching away from an IST stack lets a second
    // exception on the same vector overwrite the suspended handler's state, so
    // no inner SpinLock drop may reach the reschedule callback.
    let _ist_hold = IstPreemptHold::new(vector < 32);

    // NMI is answered before `IrqNestHold`: `interrupt_nesting_enter` stores
    // `in_interrupt` and bumps the depth as two instructions, and an NMI landing
    // between them runs an enter/exit pair whose exit clears `in_interrupt`
    // while still nested one deep. `IstPreemptHold` is one increment, so it has
    // no such window and is taken first.
    if vector == EXCEPTION_NMI {
        nmi_handler(frame_ref);
        return;
    }

    let mut irq_nest = IrqNestHold::enter();
    ist_stacks::ist_record_usage(vector, frame as u64);

    if vector == SYSCALL_VECTOR {
        answer_legacy_syscall(frame_ref);
        return;
    }

    if vector == TLB_SHOOTDOWN_VECTOR {
        handle_tlb_shootdown_ipi();
        return;
    }

    if vector == RCU_QS_IPI_VECTOR {
        slopos_ostd::sync::rcu_note_qs_from_interrupt();
        send_eoi();
        return;
    }

    if vector == RESCHEDULE_IPI_VECTOR {
        send_eoi();
        scheduler_request_reschedule(RescheduleReason::RescheduleIpi);
        irq_nest.handoff(TrapExitSource::RescheduleIpi);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
        return;
    }

    if vector == SHUTDOWN_VECTOR {
        send_eoi();
        // This CPU never runs again, so it must leave the sets that assume an
        // answer: the TLB ladder would wait on its ack, the lockup detector on
        // a tick that never comes.
        slopos_mm::tlb::notify_cpu_offline();
        slopos_arch::pcr::mark_cpu_offline(slopos_arch::get_current_cpu());
        cpu::disable_interrupts();
        cpu::halt_loop();
    }

    if vector == LAPIC_TIMER_VECTOR {
        slopos_core::irq::increment_timer_ticks();
        // EOI before the handler avoids starving the timer when the handler is
        // slow; the gate cleared IF, so the next tick stays pending until IRET
        // rather than nesting.
        send_eoi();
        slopos_sched::scheduler::scheduler_handle_timer_interrupt(frame);
        irq_nest.handoff(TrapExitSource::Irq);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
        return;
    }

    if vector >= IRQ_BASE_VECTOR {
        // Catches a registered handler scribbling through a stale pointer onto
        // our IRET frame. Only CS+RIP, and checked before the handoff, because a
        // context switch legitimately changes the other frame fields on resume.
        let expected_cs = frame_ref.cs;
        let expected_rip = frame_ref.rip;

        slopos_ostd::irq::dispatch(vector, frame_ref.error_code);

        if frame_ref.cs != expected_cs || frame_ref.rip != expected_rip {
            klog_info!(
                "IRQ: Frame corruption detected on vector {} - aborting",
                vector
            );
            kdiag_dump_interrupt_frame(frame);
            panic!("IRQ: frame corrupted");
        }

        send_eoi();
        irq_nest.handoff(TrapExitSource::Irq);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
        return;
    }

    if vector == EXCEPTION_PAGE_FAULT {
        if try_handle_page_fault(frame) {
            return;
        }
    }

    if vector == EXCEPTION_GENERAL_PROTECTION && try_handle_general_protection(frame) {
        return;
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
    let mode = ExceptionMode::load();
    if critical || !matches!(mode, ExceptionMode::Test) {
        let name = slopos_arch::arch::exception::get_exception_name(vector);
        klog_info!("EXCEPTION: Vector {} ({})", vector, name);
    }

    let mut handler = panic_handler_for(vector);
    if !critical && matches!(mode, ExceptionMode::Test) {
        if let Some(override_handler) = handler_tables::override_for(vector) {
            handler = override_handler;
        }
    }

    handler(frame);
}

fn initialize_handler_tables() {
    for vector in 0u8..32 {
        handler_tables::install_panic(vector, exception_default_panic);
    }
    handler_tables::clear_overrides();

    handler_tables::install_panic(EXCEPTION_DIVIDE_ERROR, exception_fatal);
    // NMI (vector 2) is answered in `common_exception_handler_impl`; no entry.
    handler_tables::install_panic(EXCEPTION_DOUBLE_FAULT, exception_fatal);
    handler_tables::install_panic(EXCEPTION_INVALID_TSS, exception_fatal);
    handler_tables::install_panic(EXCEPTION_SEGMENT_NOT_PRES, exception_fatal);
    handler_tables::install_panic(EXCEPTION_STACK_FAULT, exception_fatal);
    handler_tables::install_panic(EXCEPTION_MACHINE_CHECK, exception_fatal);

    handler_tables::install_panic(EXCEPTION_DEBUG, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_BREAKPOINT, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_OVERFLOW, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_BOUND_RANGE, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_FPU_ERROR, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_ALIGNMENT_CHECK, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_SIMD_FP_EXCEPTION, exception_nonfatal);

    handler_tables::install_panic(EXCEPTION_INVALID_OPCODE, exception_invalid_opcode);
    handler_tables::install_panic(EXCEPTION_DEVICE_NOT_AVAIL, exception_device_not_available);
    handler_tables::install_panic(EXCEPTION_GENERAL_PROTECTION, exception_general_protection);
    handler_tables::install_panic(EXCEPTION_PAGE_FAULT, exception_page_fault);
}

/// Called when the ISR's pre-IRETQ CS validation detects a corrupt IRET frame;
/// the pointer comes from the ISR-asm stub via `ffi_boundary`.
pub(crate) fn handle_corrupt_iret_frame(iret_frame: *const u64) -> ! {
    use slopos_ostd::arch::x86_64::kernel_ptr::{read_iret_frame, read_unaligned_u64_in_range};
    use slopos_ostd::klog_info;

    // Zeros rather than a fault: a pointer failing the canonical-kernel
    // pre-check must not nested-fault out of the dump.
    let [rip, cs, rflags, rsp, ss] = read_iret_frame(iret_frame).unwrap_or([0u64; 5]);

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

    // A fault path may take no lock and upgrade no `KArc`, so the fields come
    // from a volatile read behind the PCR's id filter rather than a lookup.
    let diag = slopos_sched::task_struct::current_task_diag();

    klog_info!("=== IRET FRAME VICINITY DUMP ===");
    let (dump_lo, dump_hi) =
        slopos_ostd::task::TaskDiag::probe_range(diag.as_ref(), iret_frame as usize, 128);
    for offset in -4isize..10 {
        // Wrapping: negative offsets deliberately leave the allocated object.
        let addr = iret_frame.wrapping_offset(offset);
        let Some(val) = read_unaligned_u64_in_range(addr, dump_lo, dump_hi) else {
            klog_info!("  [{:+}] {:p} = <out of bounds>", offset, addr);
            continue;
        };
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

    if let Some(diag) = diag.as_ref() {
        klog_info!(
            "  Current task: id={} user={}",
            diag.id,
            (diag.flags & slopos_abi::task::TASK_FLAG_USER_MODE) != 0,
        );
    }

    panic!("Unrecoverable IRET frame corruption");
}

/// Kernel-mode #GP inside the fault-recoverable XRSTOR64 band: redirect RIP to
/// the failure tail, which reports the rejection to the Rust caller. A ring-0
/// #GP is otherwise terminal, and the RIP-range match is exact, so this masks
/// no other fault.
fn try_handle_general_protection(frame: *mut slopos_arch::InterruptFrame) -> bool {
    let mut frame_anchor = ();
    let frame_ref = slopos_arch::InterruptFrame::from_ptr_mut(&mut frame_anchor, frame)
        .expect("try_handle_general_protection: null frame ptr");

    if !in_user(frame_ref) && slopos_ostd::task::fpu::is_fpu_xrstor_ip(frame_ref.rip) {
        frame_ref.rip = slopos_ostd::task::fpu::fpu_xrstor_fault_ip();
        return true;
    }

    false
}

fn try_handle_page_fault(frame: *mut slopos_arch::InterruptFrame) -> bool {
    let mut frame_anchor = ();
    let fault_addr = cpu::read_cr2();
    let frame_ref = slopos_arch::InterruptFrame::from_ptr_mut(&mut frame_anchor, frame)
        .expect("try_handle_page_fault: null frame ptr");

    // Checked before the IST guard-fault classifier: a probe deliberately reads
    // addresses it cannot prove mapped, guard pages included, and a hit there is
    // the probe failing rather than a stack overflow.
    if !in_user(frame_ref) && slopos_ostd::arch::x86_64::kernel_ptr::is_probe_read_ip(frame_ref.rip)
    {
        frame_ref.rip = slopos_ostd::arch::x86_64::kernel_ptr::probe_read_fault_ip();
        return true;
    }

    if ist_stacks::ist_guard_fault(fault_addr).is_some() {
        return false;
    }

    // Redirecting into the usercopy fault label returns a nonzero "remaining
    // bytes" to the Rust caller, which is what makes the copy primitives safe
    // against a concurrent munmap on SMP. It is the only recoverable copy band.
    if !in_user(frame_ref) {
        if slopos_ostd::user::copy::is_ostd_usercopy_ip(frame_ref.rip) {
            frame_ref.rip = slopos_ostd::user::copy::ostd_usercopy_fault_ip();
            return true;
        }
        return false;
    }
    let Some(task_ref) = resolve_user_fault_task() else {
        return false;
    };
    let vm_handle = task_ref.process_vm_handle_raw();
    let tid = task_ref.task_id;

    match slopos_mm::page_fault::try_resolve_user_fault(
        fault_addr,
        frame_ref.error_code,
        vm_handle,
        tid,
    ) {
        slopos_mm::page_fault::FaultOutcome::Resolved => true,
        // Nothing was mapped: #PF is a fault, so the IRET frame still names the
        // faulting instruction and it re-executes. The flag is left pending
        // rather than acted on — this handler runs on an IST stack, which
        // `scheduler_handoff_on_trap_exit` refuses to switch away from — so
        // what actually lets the reference holder run is the next timer
        // interrupt, whose trap exit does hand off. Each retry round is a full
        // user-mode round trip, so the loop is preemptible throughout.
        slopos_mm::page_fault::FaultOutcome::Retry => {
            scheduler_request_reschedule(RescheduleReason::InterruptWake);
            true
        }
        // Recorded rather than returned because the `bool`-shaped IST dispatcher
        // has no channel for a reason. Read and cleared by
        // `take_pending_fault_reason` on this CPU with interrupts off.
        slopos_mm::page_fault::FaultOutcome::Fatal(reason) => {
            crate::exception::record_fault_reason(reason);
            false
        }
    }
}
