#![allow(bad_asm_style)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU8, Ordering};

use slopos_arch::cpu;
use slopos_sched::task::{task_has_flag, task_id_of, task_kernel_stack_bounds, task_process_id};
// Re-export the OSTD IDT types/constants the legacy `boot::idt::*`
// surface exposed (consumed by `boot/src/tests/gdt_tests.rs` and similar).
pub use slopos_ostd::irq::{
    EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_SEGMENT_NOT_PRES, EXCEPTION_SIMD_FP_EXCEPTION,
    EXCEPTION_STACK_FAULT, IRQ_BASE_VECTOR, IdtBuilder, IdtEntry, IstPreemptHold,
    LAPIC_TIMER_VECTOR, LUF_DRAIN_IPI_VECTOR, RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR,
    SYSCALL_VECTOR, TLB_SHOOTDOWN_VECTOR,
};
use slopos_ostd::{kdiag_dump_interrupt_frame, klog_debug, klog_info};

use crate::exception::*;
use crate::ist_stacks;
use crate::user_fault::*;

// =============================================================================
// ABI razors — fail the build if a load-bearing field offset drifts.
// =============================================================================

const _: () = {
    use core::mem::offset_of;
    // CPU-pushed portion of the InterruptFrame (must match the asm
    // unwind in slopos-ostd/src/irq/asm/handlers.s).
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rip) == 136);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, cs) == 144);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rflags) == 152);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, rsp) == 160);
    assert!(offset_of!(slopos_ostd::irq::InterruptFrame, ss) == 168);
    // Per-CPU kernel RSP slot read by the OSTD `__ostd_user_return`
    // trampoline as `gs:[16]`.
    assert!(slopos_ostd::cpu::x86_64::pcr::offsets::KERNEL_RSP == 16);
};

// =============================================================================
// IDT storage — single OSTD-owned builder.
// =============================================================================

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

/// Per-vector handler tables.
///
/// Both the panic table and the override table are stored as
/// `[AtomicPtr<()>; 32]`, with null encoding "no handler installed". The
/// fn-ptr ↔ `*mut ()` round-trip lives once inside `decode`. Per-slot
/// stores avoid copying the 256-byte fn-pointer array as a single value,
/// which is what the kernel's stack-frame budget would otherwise reject.
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

use slopos_core::syscall::syscall_handle;
use slopos_drivers::apic::send_eoi;
use slopos_mm::tlb;

use slopos_sched::scheduler::{
    RescheduleReason, TrapExitSource, scheduler_handoff_on_trap_exit, scheduler_request_reschedule,
};

/// One-shot BSP-only IDT initialisation: install OSTD's default
/// handlers into the static `BUILDER` and seed our exception-handler
/// tables. The `&BspToken<'brand>` witness binds the call to the BSP-init
/// scope opened by `slopos_ostd::sync::run_bsp_init`.
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

/// Bind an IDT entry to an IST slot. `&mut BootCtx<'_, K>` gates the
/// call so production code (post-boot) cannot accidentally rebind
/// interrupt stacks. `K: CpuInitKind` keeps the surface dual-callable
/// from BSP-init, AP-init, and test scopes — every CPU brings up its
/// own IDT IST bindings.
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

/// Load the static IDT on the current CPU. Both BSP-init and per-AP
/// bringup paths call this, so it accepts any `CpuInitWitness`
/// (`BspToken` or `ApToken`) — the witness gates the call to a
/// boot-init scope without distinguishing BSP from AP.
pub fn idt_load<W: slopos_ostd::sync::CpuInitWitness>(witness: &W) {
    // BUILDER is `static` and BSP-init / AP-init ordering guarantees
    // gate population + GDT/TSS load already happened — encoded by
    // the typed `&'static IdtBuilder` + `CpuInitWitness` signature.
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

// `IstPreemptHold` is provided by OSTD's `slopos_ostd::irq` module —
// see the import at the top of this file. The boot-side path used to
// duplicate the inc/dec body; now it just borrows the canonical guard
// type.

/// NMI watchdog handler -- invoked when a neighbouring CPU sends an NMI
/// because this CPU has not recorded a timer tick for >500 ms.
///
/// Dumps the faulting context, reports held-lock state, force-unlocks all
/// tracked locks (so other CPUs are not permanently blocked), and panics.
fn nmi_watchdog_handler(frame: &slopos_arch::InterruptFrame) {
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Reliable Abort Core — panic stop-the-world. If a peer CPU has won the
    // fatal-panic election, this NMI is its stop broadcast, NOT a watchdog
    // wedge: stop cleanly and silently so the owner can drive the console
    // alone. (A self-directed NMI on the owner itself must not self-stop.)
    if slopos_ostd::panic::panic_owner_claimed()
        && !slopos_ostd::panic::panic_owner_is(cpu_id as u32)
    {
        // Force-ack any TLB shootdowns we owe so a non-panicking initiator
        // does not spin forever waiting for an ack we will never deliver
        // (we are about to halt). Set-only — never clears an ack.
        slopos_mm::tlb::force_ack_local_shootdowns(cpu_id);
        // Release every lock we hold so the owner's console/diagnostics paths
        // are not blocked behind us.
        slopos_ostd::sync::panic_recovery::poison_all_held_locks_no_halt();
        // Tell the owner we have stopped, then halt for good. No backtrace, no
        // panic, no console writes — minimal and format-free.
        slopos_ostd::panic::mark_cpu_stopped();
        slopos_arch::cpu::disable_interrupts();
        slopos_arch::cpu::halt_loop();
    }

    // Use serial logging directly to avoid lock recursion.
    klog_info!(
        "NMI WATCHDOG: CPU {} locked up! RIP={:#x} RSP={:#x} CS={:#x} RBP={:#x}",
        cpu_id,
        frame.rip,
        frame.rsp,
        frame.cs,
        frame.rbp
    );

    let held = slopos_ostd::sync::held_lock_count();
    klog_info!("NMI WATCHDOG: CPU {} holds {} lock(s)", cpu_id, held);

    // Walk the saved-frame chain via %rbp. Each frame: [saved_rbp][return_addr][...].
    // Stop on null/non-canonical/misaligned pointer or after 16 frames.
    //
    // We're in NMI watchdog context — a fault on the read_volatile here
    // would nest a #PF under an NMI panic and risk a triple-fault. Defend
    // by validating each rbp before the read:
    //   - canonical kernel half  (high 17 bits all 1)
    //   - 8-byte aligned         (frame pointer ABI requirement)
    //   - room for two u64 reads (rbp + 16 must remain canonical)
    // This isn't a tight bounds check (we can't cheaply prove rbp lies
    // inside the interrupted task's kernel stack from NMI context, where
    // we're on an IST stack), but it eliminates the obvious fault classes
    // — null, user-half, unaligned, and end-of-canonical-space wrap-around.
    {
        use slopos_ostd::arch::x86_64::kernel_ptr::read_volatile_canonical_kernel_u64;
        let mut rbp = frame.rbp;
        klog_info!("NMI WATCHDOG: CPU {} backtrace:", cpu_id);
        klog_info!("  [0] {:#x}", frame.rip);
        for depth in 1..=16u32 {
            // Validate-then-read inside `read_volatile_canonical_kernel_u64`:
            // canonical-kernel, 8-byte-aligned, 8-byte headroom on each
            // word. The second read needs an extra 8 bytes of canonical
            // headroom — re-validate by asking for the +8 offset.
            let Some(next_rbp) = read_volatile_canonical_kernel_u64(rbp) else {
                break;
            };
            let Some(ret_addr) = read_volatile_canonical_kernel_u64(rbp.wrapping_add(8)) else {
                break;
            };
            klog_info!("  [{}] {:#x}", depth, ret_addr);
            if next_rbp <= rbp {
                break;
            }
            rbp = next_rbp;
        }
    }

    // Surface the locked-up CPU's RIP/RSP/RBP on the panic screen (not just
    // the serial log) so the wedge site is visible without a serial console.
    crate::panic::set_panic_cpu_state(frame.rip, frame.rsp, frame.rbp);

    // Force-release all tracked locks so other CPUs can make progress.
    slopos_ostd::sync::panic_recovery::poison_all_held_locks_no_halt();

    panic!("NMI WATCHDOG: CPU {} not responding for >500ms", cpu_id);
}

/// Implementation of common_exception_handler - called from FFI boundary
pub fn common_exception_handler_impl(frame: *mut slopos_arch::InterruptFrame) {
    let frame_ref = slopos_arch::InterruptFrame::from_ptr_mut(frame)
        .expect("common_exception_handler_impl: null frame ptr");
    let vector = (frame_ref.vector & 0xFF) as u8;

    // Prevent deferred rescheduling during IST-based exception handlers.
    //
    // IST stacks are per-vector, per-CPU fixed addresses.  If an SpinLock guard
    // drops while preempt_count is 1, the PreemptGuard::drop callback will call
    // the scheduler, context-switching away from the IST stack.  A subsequent
    // exception of the same vector would reuse the same IST stack, overwriting
    // the suspended handler's state → corruption / triple fault.
    //
    // By bumping preempt_count here (and manually decrementing on exit WITHOUT
    // calling the reschedule callback), we ensure all inner SpinLock drops see
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
        // Legacy `int 0x80` syscall path. SlopOS userland uses the
        // SYSCALL instruction (LSTAR → `__ostd_user_return`) for every
        // real syscall, so this trap is rarely taken in practice. We
        // still bridge the InterruptFrame here onto a transient OSTD
        // `UserContext` so any caller that does take this path observes
        // identical syscall semantics — including the CS/SS/RFLAGS-mask
        // discipline that `set_regs` enforces on the modern syscall
        // entry.
        use slopos_ostd::user::context::{FpuStateRef, UserContext, UserRegs};
        let mut user_regs = UserRegs::default();
        user_regs.r15 = frame_ref.r15;
        user_regs.r14 = frame_ref.r14;
        user_regs.r13 = frame_ref.r13;
        user_regs.r12 = frame_ref.r12;
        user_regs.r11 = frame_ref.r11;
        user_regs.r10 = frame_ref.r10;
        user_regs.r9 = frame_ref.r9;
        user_regs.r8 = frame_ref.r8;
        user_regs.rbp = frame_ref.rbp;
        user_regs.rdi = frame_ref.rdi;
        user_regs.rsi = frame_ref.rsi;
        user_regs.rdx = frame_ref.rdx;
        user_regs.rcx = frame_ref.rcx;
        user_regs.rbx = frame_ref.rbx;
        user_regs.rax = frame_ref.rax;
        user_regs.rip = frame_ref.rip;
        user_regs.rsp = frame_ref.rsp;
        user_regs.rflags_user_subset = frame_ref.rflags;
        let mut user_ctx = UserContext::new(user_regs, FpuStateRef::empty());
        syscall_handle(&mut user_ctx as *mut UserContext);
        // Apply the handler's mutations back onto the IRET frame so the
        // CPU sees the new register state on `iretq`.
        let new_regs = user_ctx.regs();
        frame_ref.r15 = new_regs.r15;
        frame_ref.r14 = new_regs.r14;
        frame_ref.r13 = new_regs.r13;
        frame_ref.r12 = new_regs.r12;
        frame_ref.r11 = new_regs.r11;
        frame_ref.r10 = new_regs.r10;
        frame_ref.r9 = new_regs.r9;
        frame_ref.r8 = new_regs.r8;
        frame_ref.rbp = new_regs.rbp;
        frame_ref.rdi = new_regs.rdi;
        frame_ref.rsi = new_regs.rsi;
        frame_ref.rdx = new_regs.rdx;
        frame_ref.rcx = new_regs.rcx;
        frame_ref.rbx = new_regs.rbx;
        frame_ref.rax = new_regs.rax;
        frame_ref.rip = new_regs.rip;
        frame_ref.rsp = new_regs.rsp;
        frame_ref.rflags = new_regs.rflags_user_subset;
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
        slopos_ostd::sync::rcu_note_qs();
        send_eoi();
        return;
    }

    if vector == RESCHEDULE_IPI_VECTOR {
        send_eoi();
        scheduler_request_reschedule(RescheduleReason::RescheduleIpi);
        scheduler_handoff_on_trap_exit(TrapExitSource::RescheduleIpi);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
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
        // EOI before handler: prevents timer starvation if the handler is
        // slow (e.g. blocked on a lock). Safe because the interrupt gate
        // clears IF, so the next timer interrupt won't nest — it stays
        // pending until IRET re-enables interrupts.
        send_eoi();
        slopos_sched::scheduler::scheduler_handle_timer_interrupt(frame);
        scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
        return;
    }

    if vector >= IRQ_BASE_VECTOR {
        // Snapshot the (CS, RIP) pair before the dispatch closure runs so
        // we can detect a registered handler scribbling through a stale
        // pointer onto our IRET frame. Mirrors the legacy
        // `core::irq::irq_dispatch` check both in scope (just CS+RIP, not
        // the full 5-field IRET payload) and in ordering (the check runs
        // *before* EOI + scheduler_handoff_on_trap_exit, since handoff
        // may legitimately context-switch and the IRET frame's RFLAGS /
        // RSP / SS shape on resume is not byte-identical to the pre-IRQ
        // snapshot in every user-mode-preemption edge case).
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
        scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
        slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
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
    // Default fallback for the entire vector range.
    for vector in 0u8..32 {
        handler_tables::install_panic(vector, exception_default_panic);
    }
    handler_tables::clear_overrides();

    // Fatal: log name, dump frame, panic.
    handler_tables::install_panic(EXCEPTION_DIVIDE_ERROR, exception_fatal);
    // NMI (vector 2) is handled directly in common_exception_handler_impl
    // by the watchdog handler -- no table entry needed.
    handler_tables::install_panic(EXCEPTION_DOUBLE_FAULT, exception_fatal);
    handler_tables::install_panic(EXCEPTION_INVALID_TSS, exception_fatal);
    handler_tables::install_panic(EXCEPTION_SEGMENT_NOT_PRES, exception_fatal);
    handler_tables::install_panic(EXCEPTION_STACK_FAULT, exception_fatal);
    handler_tables::install_panic(EXCEPTION_MACHINE_CHECK, exception_fatal);

    // Non-fatal: log name, dump frame, resume.
    handler_tables::install_panic(EXCEPTION_DEBUG, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_BREAKPOINT, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_OVERFLOW, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_BOUND_RANGE, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_FPU_ERROR, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_ALIGNMENT_CHECK, exception_nonfatal);
    handler_tables::install_panic(EXCEPTION_SIMD_FP_EXCEPTION, exception_nonfatal);

    // Specialized: user-mode check before fatal/nonfatal fallback.
    handler_tables::install_panic(EXCEPTION_INVALID_OPCODE, exception_invalid_opcode);
    handler_tables::install_panic(EXCEPTION_DEVICE_NOT_AVAIL, exception_device_not_available);
    handler_tables::install_panic(EXCEPTION_GENERAL_PROTECTION, exception_general_protection);
    handler_tables::install_panic(EXCEPTION_PAGE_FAULT, exception_page_fault);
}

/// Called when the ISR's pre-IRETQ CS validation detects a corrupt IRET
/// frame.  Logs the corruption for debugging, then panics.
///
/// Safe-fn surface: the OSTD helper `read_unaligned_u64` centralises
/// the `read_unaligned` unsafe; the caller-supplied pointer arrives
/// from the ISR-asm stub (described by the FFI boundary contract in
/// `ffi_boundary::isr_iret_frame_corrupt`), so the `pub(crate)` shim
/// here only needs to forward.
pub(crate) fn handle_corrupt_iret_frame(iret_frame: *const u64) -> ! {
    use slopos_ostd::arch::x86_64::kernel_ptr::{read_iret_frame, read_unaligned_u64_in_range};
    use slopos_ostd::klog_info;

    // Safe helper centralises the `read_unaligned`. If the ISR-asm
    // pointer fails the canonical-kernel pre-check (should not happen
    // for a real corrupt-frame trap), fall back to zeros so the dump
    // can still proceed without nested-faulting.
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

    // Dump the surrounding stack words for forensics.
    // Limit the probe range to avoid nested faults on invalid addresses:
    // use the current task's kernel stack bounds when available, otherwise
    // conservatively bound to ±128 bytes around iret_frame.
    // Call scheduler_get_current_task() once and reuse for both the
    // bounds computation and the task-info dump below.
    let task_ptr = slopos_sched::scheduler::scheduler_get_current_task();

    klog_info!("=== IRET FRAME VICINITY DUMP ===");
    let (dump_lo, dump_hi) = match task_kernel_stack_bounds(task_ptr) {
        Some((base, top)) if base != 0 && top > base => (base as usize, top as usize),
        _ => {
            let mid = iret_frame as usize;
            (mid.saturating_sub(128), mid.saturating_add(128))
        }
    };
    for offset in -4isize..10 {
        // Use wrapping_offset to avoid UB when the resulting pointer
        // falls outside the allocated object (negative offsets).
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

    if !task_ptr.is_null() {
        let is_user = task_has_flag(task_ptr, slopos_abi::task::TASK_FLAG_USER_MODE);
        klog_info!(
            "  Current task: id={} user={}",
            task_id_of(task_ptr).unwrap_or(0),
            is_user,
        );
    }

    // User-mode IRET corruption is fatal — a corrupted frame cannot be
    // recovered and the panic below fires regardless.
    panic!("Unrecoverable IRET frame corruption");
}

fn try_handle_page_fault(frame: *mut slopos_arch::InterruptFrame) -> bool {
    let fault_addr = cpu::read_cr2();
    let frame_ref = slopos_arch::InterruptFrame::from_ptr_mut(frame)
        .expect("try_handle_page_fault: null frame ptr");

    // Fault-recoverable kernel probe read (diagnostic walkers). Checked
    // before the IST guard-fault classifier: a probe deliberately reads
    // addresses it cannot prove mapped — including stack guard pages —
    // and a hit there is the probe failing, not a stack overflow. The
    // RIP-range match is exact, so this cannot mask a real fault.
    if !in_user(frame_ref) && slopos_ostd::arch::x86_64::kernel_ptr::is_probe_read_ip(frame_ref.rip)
    {
        frame_ref.rip = slopos_ostd::arch::x86_64::kernel_ptr::probe_read_fault_ip();
        return true;
    }

    if ist_stacks::ist_guard_fault(fault_addr).is_some() {
        return false;
    }

    // Kernel-mode fault inside the OSTD usercopy assembly region:
    // recover gracefully by redirecting RIP to the fault return
    // label, which returns a nonzero "remaining bytes" value to the
    // Rust caller. This makes the user-copy primitives safe against
    // concurrent munmap on SMP (Redox OS pattern). The OSTD region
    // is the only fault-recoverable copy band in the kernel — there
    // is no parallel `slopos_mm` asm shim anymore.
    if !in_user(frame_ref) {
        if slopos_ostd::user::copy::is_ostd_usercopy_ip(frame_ref.rip) {
            frame_ref.rip = slopos_ostd::user::copy::ostd_usercopy_fault_ip();
            return true;
        }
        return false;
    }
    let task_ptr = resolve_user_fault_task();
    if task_ptr.is_null() {
        return false;
    }
    let pid = task_process_id(task_ptr).unwrap_or(0);
    let tid = task_id_of(task_ptr).unwrap_or(0);

    slopos_mm::page_fault::try_resolve_user_fault(fault_addr, frame_ref.error_code, pid, tid)
}
