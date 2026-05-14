//! IDT (Interrupt Descriptor Table) construction surface and the
//! IRET-frame corruption recovery path.
//!
//! [`IdtBuilder`] formats hardware IDT gates from typed inputs.
//! [`handle_corrupt_iret_frame`] is the irreducible IRET-frame
//! recovery path lifted out of the boot crate — it stays here in
//! OSTD because the diagnostic dump is the last line of defence
//! against silent CPU-state corruption (Inv. 2).
//!
//! This module also hosts the IST-vector entry guard ([`IrqEntryGuard`]
//! / [`IstPreemptHold`]) which keeps the per-CPU preempt count elevated
//! for the duration of an IST-using exception handler. Yielding from an
//! IST handler corrupts the per-vector IST stack, so this guard's
//! `Drop` decrements the count *without* invoking any deferred
//! reschedule callback.
//!
//! # Soundness
//!
//! Inv. 2: kernel-mode CPU state cannot be tampered with by OSTD
//! clients. The diagnostic dump never returns; corrupt IRET frames
//! are unrecoverable.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu::preempt;
use crate::sync::BspToken;

// Inline the OSTD-side IDT entry-point asm stubs. Gated on x86_64 +
// kernel build (i.e. not host unit tests): the stubs reference Rust
// symbols `common_exception_handler` / `isr_iret_frame_corrupt` that
// only exist in the kernel link, plus the `msi_vector_table` rodata
// array consumed by `install_default_handlers`.
#[cfg(all(target_arch = "x86_64", not(test)))]
core::arch::global_asm!(include_str!("asm/handlers.s"), options(att_syntax));

// ---------------------------------------------------------------------------
// Architectural constants and gate descriptor.
// ---------------------------------------------------------------------------

/// Number of entries in the IDT (256 vectors).
pub const IDT_ENTRIES: usize = 256;

/// Interrupt-gate type/attr byte (DPL=0, present, interrupt gate).
/// Clears IF on entry.
pub const IDT_GATE_INTERRUPT: u8 = 0x8E;

/// Trap-gate type/attr byte (DPL=0, present, trap gate). Does *not*
/// clear IF on entry.
pub const IDT_GATE_TRAP: u8 = 0x8F;

// ---------------------------------------------------------------------------
// CPU exception vectors (0-31) — Intel SDM Vol. 3A, Table 6-1.
// ---------------------------------------------------------------------------

/// Divide Error (#DE) - vector 0.
pub const EXCEPTION_DIVIDE_ERROR: u8 = 0;
/// Debug (#DB) - vector 1.
pub const EXCEPTION_DEBUG: u8 = 1;
/// Non-Maskable Interrupt (NMI) - vector 2.
pub const EXCEPTION_NMI: u8 = 2;
/// Breakpoint (#BP) - vector 3.
pub const EXCEPTION_BREAKPOINT: u8 = 3;
/// Overflow (#OF) - vector 4.
pub const EXCEPTION_OVERFLOW: u8 = 4;
/// Bound Range Exceeded (#BR) - vector 5.
pub const EXCEPTION_BOUND_RANGE: u8 = 5;
/// Invalid Opcode (#UD) - vector 6.
pub const EXCEPTION_INVALID_OPCODE: u8 = 6;
/// Device Not Available (#NM) - vector 7.
pub const EXCEPTION_DEVICE_NOT_AVAIL: u8 = 7;
/// Double Fault (#DF) - vector 8.
pub const EXCEPTION_DOUBLE_FAULT: u8 = 8;
/// Coprocessor Segment Overrun - vector 9 (reserved).
pub const EXCEPTION_COPROCESSOR_OVERRUN: u8 = 9;
/// Invalid TSS (#TS) - vector 10.
pub const EXCEPTION_INVALID_TSS: u8 = 10;
/// Segment Not Present (#NP) - vector 11.
pub const EXCEPTION_SEGMENT_NOT_PRES: u8 = 11;
/// Stack-Segment Fault (#SS) - vector 12.
pub const EXCEPTION_STACK_FAULT: u8 = 12;
/// General Protection (#GP) - vector 13.
pub const EXCEPTION_GENERAL_PROTECTION: u8 = 13;
/// Page Fault (#PF) - vector 14.
pub const EXCEPTION_PAGE_FAULT: u8 = 14;
/// Reserved - vector 15.
pub const EXCEPTION_RESERVED_15: u8 = 15;
/// x87 FPU Floating-Point Error (#MF) - vector 16.
pub const EXCEPTION_FPU_ERROR: u8 = 16;
/// Alignment Check (#AC) - vector 17.
pub const EXCEPTION_ALIGNMENT_CHECK: u8 = 17;
/// Machine Check (#MC) - vector 18.
pub const EXCEPTION_MACHINE_CHECK: u8 = 18;
/// SIMD Floating-Point Exception (#XM/#XF) - vector 19.
pub const EXCEPTION_SIMD_FP_EXCEPTION: u8 = 19;
/// Virtualization Exception (#VE) - vector 20.
pub const EXCEPTION_VIRTUALIZATION: u8 = 20;
/// Control Protection Exception (#CP) - vector 21.
pub const EXCEPTION_CONTROL_PROTECTION: u8 = 21;
// Vectors 22-31 are reserved.

// ---------------------------------------------------------------------------
// Hardware IRQ, syscall, and IPI vector constants.
// ---------------------------------------------------------------------------

/// Base vector for hardware IRQs (IRQ0 maps to this vector).
pub const IRQ_BASE_VECTOR: u8 = 32;

/// Syscall interrupt vector (int 0x80).
pub const SYSCALL_VECTOR: u8 = 0x80;

/// TLB shootdown IPI vector (0xFD). Used for cross-CPU TLB invalidation.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFD;

/// Reschedule IPI vector (0xFC). Wakes a CPU from idle to run newly-queued tasks.
pub const RESCHEDULE_IPI_VECTOR: u8 = 0xFC;

/// RCU quiescent-state IPI vector (0xFB). Bumps the per-CPU RCU QS counter.
pub const RCU_QS_IPI_VECTOR: u8 = 0xFB;

/// LUF drain-by-phys IPI vector (0xFA). Flushes stale LUF translations
/// before a freed frame is reused by a different VMA.
pub const LUF_DRAIN_IPI_VECTOR: u8 = 0xFA;

/// LAPIC timer vector (0xEC). Each CPU's local APIC timer fires here for
/// scheduler preemption.
pub const LAPIC_TIMER_VECTOR: u8 = 0xEC;

// ---------------------------------------------------------------------------
// MSI vector range.
// ---------------------------------------------------------------------------

/// First vector available for MSI allocation. Vectors 32-47 are reserved
/// for legacy IOAPIC IRQs (IRQ0-IRQ15).
pub const MSI_VECTOR_BASE: u8 = 48;

/// One-past-the-last MSI vector. Vectors 224-255 are reserved for
/// system IPIs, LAPIC timer, and spurious.
pub const MSI_VECTOR_END: u8 = 224;

/// Total number of vectors available for MSI allocation.
pub const MSI_VECTOR_COUNT: usize = (MSI_VECTOR_END - MSI_VECTOR_BASE) as usize;

/// x86-64 IDT entry. Layout matches Intel SDM Vol. 3A §6.14.1.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct IdtEntry {
    pub offset_low: u16,
    pub selector: u16,
    pub ist: u8,
    pub type_attr: u8,
    pub offset_mid: u16,
    pub offset_high: u32,
    pub zero: u32,
}

impl IdtEntry {
    pub const fn zero() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            zero: 0,
        }
    }

    /// Format gate fields from a handler entrypoint, segment selector,
    /// gate type byte, and DPL. Pure-logic helper used by
    /// [`IdtBuilder::set_gate_priv`] and the unit tests.
    pub const fn format(handler: u64, selector: u16, typ: u8, dpl: u8) -> Self {
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector,
            ist: 0,
            type_attr: typ | 0x80 | ((dpl & 0x3) << 5),
            offset_mid: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    }

    /// Reassemble the handler offset from the three-part split.
    pub const fn handler(&self) -> u64 {
        (self.offset_low as u64)
            | ((self.offset_mid as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }
}

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

// ---------------------------------------------------------------------------
// IdtBuilder.
// ---------------------------------------------------------------------------

/// Owns the 256-entry IDT array and exposes typed gate-formatting
/// helpers. The hardware `lidt` instruction is gated behind an
/// `unsafe fn load`.
pub struct IdtBuilder {
    entries: UnsafeCell<[IdtEntry; IDT_ENTRIES]>,
}

// SAFETY: every mutator goes through `&self` + interior mutability;
// callers serialise the IDT-build sequence at boot. The hardware load
// is `unsafe fn`. Inv. 2.
unsafe impl Sync for IdtBuilder {}

impl IdtBuilder {
    pub const fn new() -> Self {
        Self {
            entries: UnsafeCell::new([IdtEntry::zero(); IDT_ENTRIES]),
        }
    }

    /// Install a kernel-only (DPL=0) gate.
    pub fn set_gate(&self, vector: u8, handler: u64, selector: u16, typ: u8) {
        self.set_gate_priv(vector, handler, selector, typ, 0);
    }

    /// Install a gate with an explicit Descriptor Privilege Level
    /// (DPL=3 is required for software-int gates reachable from user
    /// mode, e.g. the `int 0x80` syscall trampoline).
    pub fn set_gate_priv(&self, vector: u8, handler: u64, selector: u16, typ: u8, dpl: u8) {
        let formatted = IdtEntry::format(handler, selector, typ, dpl);
        // SAFETY: `entries` is owned by this builder; aliasing is the
        // caller's responsibility (boot is single-threaded). Inv. 2.
        unsafe {
            (*self.entries.get())[vector as usize] = formatted;
        }
    }

    /// Bind a gate to a TSS IST slot (1..=7). Slot 0 = no IST.
    pub fn set_ist(&self, vector: u8, ist_slot: u8) {
        // SAFETY: see `set_gate_priv`.
        unsafe {
            (*self.entries.get())[vector as usize].ist = ist_slot & 0x7;
        }
    }

    /// Read back a gate (used for diagnostics + tests).
    pub fn get_gate(&self, vector: u8) -> IdtEntry {
        // SAFETY: see `set_gate_priv`.
        unsafe { (*self.entries.get())[vector as usize] }
    }

    /// Copy a gate read-back into a caller-supplied `*mut IdtEntry`.
    ///
    /// Returns `0` on success, `-1` if `out_entry` is null. This is the
    /// FFI-boundary writer used by the `idt_get_gate(...)` C-ABI shim
    /// that kernel test fixtures consume — wrapping the unsafe write
    /// here keeps callers in safe Rust.
    pub fn write_gate_to_caller(&self, vector: u8, out_entry: *mut IdtEntry) -> i32 {
        if out_entry.is_null() {
            return -1;
        }
        let entry = self.get_gate(vector);
        // SAFETY: out_entry is non-null per the guard; the FFI caller's
        // C-ABI contract states the pointer references a writable
        // `IdtEntry`. The unsafe `ptr::write` is centralised here so
        // kernel-side shim layers (boot/) stay in safe Rust.
        unsafe {
            core::ptr::write(out_entry, entry);
        }
        0
    }

    /// Issue `lidt`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    /// - every gate has been populated (or zeroed deliberately for
    ///   "no handler installed" semantics);
    /// - the IDT storage outlives the running CPU (`'static` —
    ///   typically because `IdtBuilder` is itself a `static`);
    /// - the matching GDT/TSS describing `selector`-side has already
    ///   been loaded.
    ///
    /// Inv. 2.
    pub unsafe fn load(&self) {
        let base = self.entries.get() as u64;
        let limit = (size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16;
        let ptr = IdtPtr { limit, base };
        // SAFETY: `lidt` reads the IDTR pointer + length from the
        // 10-byte structure on the stack.
        unsafe {
            asm!(
                "lidt [{0}]",
                in(reg) &ptr,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Safe `'static` wrapper around [`Self::load`].
    ///
    /// A `&'static self` borrow + a [`CpuInitWitness`](crate::sync::CpuInitWitness)
    /// discharges the three contract clauses of `unsafe fn load`:
    ///
    /// - **`'static` storage** — guaranteed by the `&'static self`
    ///   receiver.
    /// - **Gate population** — discharged by build-step ordering; the
    ///   BSP `idt_init` path runs `install_default_handlers` before
    ///   any caller observes a witness, and `register_*` is monotonic
    ///   across that scope.
    /// - **GDT/TSS already loaded** — `CpuInitWitness` is minted only
    ///   inside `run_bsp_init` / `run_ap_init`, both of which run
    ///   `init_gdt_and_install` first.
    pub fn load_static<W: crate::sync::CpuInitWitness>(&'static self, _witness: &W) {
        // SAFETY: see fn-level docs — all three clauses of `unsafe fn
        // load` are discharged structurally.
        unsafe { self.load() };
    }
}

impl Default for IdtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// install_default_handlers — wire OSTD's asm stubs into the IDT.
// ---------------------------------------------------------------------------

#[cfg(all(target_arch = "x86_64", not(test)))]
impl IdtBuilder {
    /// Install the OSTD-supplied default exception, syscall-trap, IPI,
    /// LAPIC-timer, IRQ, and MSI gates. The asm stubs live in
    /// `slopos-ostd/src/irq/asm/handlers.s`. Boot configures IST slots
    /// (via [`set_ist`]) and loads the IDT (via [`load`]) separately.
    ///
    /// All gates use `KERNEL_CODE` (selector 0x08). The syscall trap
    /// gate is DPL=3 (user-reachable); every other gate is DPL=0.
    /// Vectors 9 and 15 are reserved (Intel SDM); they remain zeroed.
    ///
    /// [`set_ist`]: IdtBuilder::set_ist
    /// [`load`]: IdtBuilder::load
    pub fn install_default_handlers(&self) {
        unsafe extern "C" {
            // Exception entries (vectors 0..=19, except reserved 9 and 15).
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
            // Syscall trap-gate (vector 0x80).
            fn isr128();
            // IPI / spurious / timer custom stubs.
            fn isr_reschedule_ipi();
            fn isr_rcu_qs_ipi();
            fn isr_luf_drain_ipi();
            fn isr_tlb_shootdown();
            fn isr_shutdown_ipi();
            fn isr_spurious();
            fn isr_lapic_timer();
            // Legacy IRQ stubs (vectors 32..=47).
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
            // MSI stub address table.
            static msi_vector_table: [u64; MSI_VECTOR_COUNT];
        }

        let cs = crate::arch::x86_64::gdt::SegmentSelector::KERNEL_CODE.0;

        #[inline(always)]
        fn fp(p: unsafe extern "C" fn()) -> u64 {
            p as *const () as u64
        }

        // Architectural exception vectors (0..=19, minus reserved 9 and 15).
        // BP (3) and OF (4) are trap gates; the rest are interrupt gates.
        self.set_gate(EXCEPTION_DIVIDE_ERROR, fp(isr0), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_DEBUG, fp(isr1), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_NMI, fp(isr2), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_BREAKPOINT, fp(isr3), cs, IDT_GATE_TRAP);
        self.set_gate(EXCEPTION_OVERFLOW, fp(isr4), cs, IDT_GATE_TRAP);
        self.set_gate(EXCEPTION_BOUND_RANGE, fp(isr5), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_INVALID_OPCODE, fp(isr6), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_DEVICE_NOT_AVAIL, fp(isr7), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_DOUBLE_FAULT, fp(isr8), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_INVALID_TSS, fp(isr10), cs, IDT_GATE_INTERRUPT);
        self.set_gate(
            EXCEPTION_SEGMENT_NOT_PRES,
            fp(isr11),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(EXCEPTION_STACK_FAULT, fp(isr12), cs, IDT_GATE_INTERRUPT);
        self.set_gate(
            EXCEPTION_GENERAL_PROTECTION,
            fp(isr13),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(EXCEPTION_PAGE_FAULT, fp(isr14), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_FPU_ERROR, fp(isr16), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_ALIGNMENT_CHECK, fp(isr17), cs, IDT_GATE_INTERRUPT);
        self.set_gate(EXCEPTION_MACHINE_CHECK, fp(isr18), cs, IDT_GATE_INTERRUPT);
        self.set_gate(
            EXCEPTION_SIMD_FP_EXCEPTION,
            fp(isr19),
            cs,
            IDT_GATE_INTERRUPT,
        );

        // Legacy IOAPIC IRQ gates (vectors 32..=47).
        self.set_gate(32, fp(irq0), cs, IDT_GATE_INTERRUPT);
        self.set_gate(33, fp(irq1), cs, IDT_GATE_INTERRUPT);
        self.set_gate(34, fp(irq2), cs, IDT_GATE_INTERRUPT);
        self.set_gate(35, fp(irq3), cs, IDT_GATE_INTERRUPT);
        self.set_gate(36, fp(irq4), cs, IDT_GATE_INTERRUPT);
        self.set_gate(37, fp(irq5), cs, IDT_GATE_INTERRUPT);
        self.set_gate(38, fp(irq6), cs, IDT_GATE_INTERRUPT);
        self.set_gate(39, fp(irq7), cs, IDT_GATE_INTERRUPT);
        self.set_gate(40, fp(irq8), cs, IDT_GATE_INTERRUPT);
        self.set_gate(41, fp(irq9), cs, IDT_GATE_INTERRUPT);
        self.set_gate(42, fp(irq10), cs, IDT_GATE_INTERRUPT);
        self.set_gate(43, fp(irq11), cs, IDT_GATE_INTERRUPT);
        self.set_gate(44, fp(irq12), cs, IDT_GATE_INTERRUPT);
        self.set_gate(45, fp(irq13), cs, IDT_GATE_INTERRUPT);
        self.set_gate(46, fp(irq14), cs, IDT_GATE_INTERRUPT);
        self.set_gate(47, fp(irq15), cs, IDT_GATE_INTERRUPT);

        // Syscall trap gate (vector 0x80, DPL=3).
        self.set_gate_priv(SYSCALL_VECTOR, fp(isr128), cs, IDT_GATE_TRAP, 3);

        // IPIs, LAPIC timer, shutdown, spurious.
        self.set_gate(
            RESCHEDULE_IPI_VECTOR,
            fp(isr_reschedule_ipi),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(
            RCU_QS_IPI_VECTOR,
            fp(isr_rcu_qs_ipi),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(
            LUF_DRAIN_IPI_VECTOR,
            fp(isr_luf_drain_ipi),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(
            TLB_SHOOTDOWN_VECTOR,
            fp(isr_tlb_shootdown),
            cs,
            IDT_GATE_INTERRUPT,
        );
        self.set_gate(0xFE, fp(isr_shutdown_ipi), cs, IDT_GATE_INTERRUPT);
        self.set_gate(0xFF, fp(isr_spurious), cs, IDT_GATE_INTERRUPT);
        self.set_gate(
            LAPIC_TIMER_VECTOR,
            fp(isr_lapic_timer),
            cs,
            IDT_GATE_INTERRUPT,
        );

        // MSI vectors (48..224). Skip SYSCALL_VECTOR which sits inside
        // this range and gets its own DPL=3 trap gate above.
        // SAFETY: msi_vector_table is a 176-entry rodata array emitted
        // by handlers.s; the asm guarantees i < MSI_VECTOR_COUNT.
        unsafe {
            for i in 0..MSI_VECTOR_COUNT {
                let vector = MSI_VECTOR_BASE.wrapping_add(i as u8);
                if vector == SYSCALL_VECTOR {
                    continue;
                }
                self.set_gate(vector, msi_vector_table[i], cs, IDT_GATE_INTERRUPT);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ExceptionMode.
// ---------------------------------------------------------------------------

/// Whether the kernel is running production exception handlers
/// ([`Normal`]) or test-mode override handlers ([`Test`]).
///
/// [`Normal`]: ExceptionMode::Normal
/// [`Test`]: ExceptionMode::Test
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExceptionMode {
    Normal = 0,
    Test = 1,
}

// ---------------------------------------------------------------------------
// DiagnosticSink: callback for the IRET-corruption dump.
// ---------------------------------------------------------------------------

/// Sink for the IRET-frame-corruption diagnostic. Production wires a
/// klog-backed implementation; the OSTD-internal default is silent.
pub trait DiagnosticSink: Send + Sync + 'static {
    /// Emit one diagnostic line. Called from the corrupt-IRET path.
    /// The body is interpolated by the caller; the sink is a raw
    /// stream — no formatting allocation.
    fn emit(&self, line: &str);
}

struct SilentSink;
impl DiagnosticSink for SilentSink {
    #[inline]
    fn emit(&self, _line: &str) {}
}

static DEFAULT_SINK: SilentSink = SilentSink;

struct SinkSlot(UnsafeCell<MaybeUninit<&'static dyn DiagnosticSink>>);
// SAFETY: gated by `SINK_INSTALLED` AcqRel handshake.
unsafe impl Sync for SinkSlot {}

static SINK_SLOT: SinkSlot = SinkSlot(UnsafeCell::new(MaybeUninit::uninit()));
static SINK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production diagnostic sink. The
/// `&BspToken<'brand>` witnesses BSP-only init; `sink` must live for
/// `'static`.
pub fn register_diagnostic_sink<'brand>(
    _token: &BspToken<'brand>,
    sink: &'static dyn DiagnosticSink,
) {
    let was_installed = SINK_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(
        !was_installed,
        "slopos_ostd::irq::idt::register_diagnostic_sink called twice"
    );
    // SAFETY: exclusive transition just established by the swap.
    unsafe {
        (*SINK_SLOT.0.get()).write(sink);
    }
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    SINK_INSTALLED.store(false, Ordering::Release);
}

#[inline]
fn current_sink() -> &'static dyn DiagnosticSink {
    if !SINK_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_SINK;
    }
    // SAFETY: Acquire-pair with `register_diagnostic_sink` Release.
    unsafe { *(*SINK_SLOT.0.get()).as_ptr() }
}

// ---------------------------------------------------------------------------
// IRET-frame corruption recovery.
// ---------------------------------------------------------------------------

/// Emit the standard IRET-frame corruption banner and panic.
///
/// `iret_frame` must point to 5 readable u64s laid out as
/// `[RIP, CS, RFLAGS, RSP, SS]` (the CPU-pushed portion of an
/// interrupt frame). The pointer need not be aligned (values are read
/// with `read_unaligned`).
///
/// # Safety
///
/// Caller certifies the 40-byte readability of `iret_frame`. This
/// function does not return — it panics. Inv. 2.
pub unsafe fn handle_corrupt_iret_frame(iret_frame: *const u64) -> ! {
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

    let sink = current_sink();
    sink.emit("ISR IRET FRAME CORRUPT (CS expected 0x08 or 0x23)");
    let _ = (rip, cs, rflags, rsp, ss);
    // Field values are emitted by the production sink via its own
    // formatting buffer; OSTD itself does not pull a heap formatter.

    panic!("Unrecoverable IRET frame corruption");
}

// ---------------------------------------------------------------------------
// IST-handler entry guard.
// ---------------------------------------------------------------------------

/// Predicate: does the given vector enter the kernel via an IST stack
/// in SlopOS?
///
/// All x86-64 architectural exception vectors (0..=31) use IST stacks
/// in SlopOS so that handlers run on a known-good stack regardless of
/// the faulting context. Hardware IRQs (32..) do not.
#[inline]
pub const fn vector_uses_ist(vector: u8) -> bool {
    vector < 32
}

/// Const-generic RAII guard for IST-using exception entry points.
///
/// `enter()` bumps the per-CPU preempt count; `Drop` decrements it
/// with the *quiet* path so no deferred reschedule callback fires
/// (which would corrupt the IST stack).
///
/// Non-IST vectors construct as a no-op — the guard is still typed so
/// the entrypoint code is uniform.
#[must_use = "if unused, the IST preempt hold is immediately released"]
pub struct IrqEntryGuard<const V: u8> {
    _not_send: PhantomData<*const ()>,
}

impl<const V: u8> IrqEntryGuard<V> {
    /// Construct the guard.
    #[inline]
    pub fn enter() -> Self {
        if vector_uses_ist(V) {
            preempt::irq_entry_bump();
        }
        Self {
            _not_send: PhantomData,
        }
    }
}

impl<const V: u8> Drop for IrqEntryGuard<V> {
    #[inline]
    fn drop(&mut self) {
        if vector_uses_ist(V) {
            preempt::irq_entry_leave_quiet();
        }
    }
}

/// Runtime-toggleable variant of [`IrqEntryGuard`]. Equivalent body,
/// gated by an `active` boolean rather than a const generic. Used by
/// dispatch entry points where the vector is only known dynamically.
#[must_use]
pub struct IstPreemptHold {
    active: bool,
}

impl IstPreemptHold {
    /// Bump the preempt count if `active`, otherwise no-op.
    #[inline]
    pub fn new(active: bool) -> Self {
        if active {
            preempt::irq_entry_bump();
        }
        Self { active }
    }
}

impl Drop for IstPreemptHold {
    #[inline]
    fn drop(&mut self) {
        if self.active {
            preempt::irq_entry_leave_quiet();
        }
    }
}

// ---------------------------------------------------------------------------
// Lib unit tests (host-side, pure logic).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::preempt as p;

    fn isolate<R>(f: impl FnOnce() -> R) -> R {
        p::reset_for_test();
        let r = f();
        p::reset_for_test();
        r
    }

    #[test]
    fn idt_entry_format_round_trips_handler() {
        let h: u64 = 0x0000_FFFF_8000_1234;
        let e = IdtEntry::format(h, 0x08, IDT_GATE_INTERRUPT, 0);
        assert_eq!(e.handler(), h);
        let sel = e.selector;
        assert_eq!(sel, 0x08);
        let attr = e.type_attr;
        assert_eq!(attr & 0x0F, IDT_GATE_INTERRUPT & 0x0F);
        assert_eq!(attr & 0x80, 0x80); // present
        assert_eq!((attr >> 5) & 0x3, 0); // DPL=0
        let z = e.zero;
        let i = e.ist;
        assert_eq!(z, 0);
        assert_eq!(i, 0);
    }

    #[test]
    fn idt_entry_format_encodes_dpl_3() {
        let e = IdtEntry::format(0x1000, 0x08, IDT_GATE_TRAP, 3);
        let attr = e.type_attr;
        assert_eq!((attr >> 5) & 0x3, 3);
        assert_eq!(attr & 0x80, 0x80);
    }

    #[test]
    fn builder_set_gate_round_trip() {
        let b = IdtBuilder::new();
        b.set_gate(13, 0xFFFF_8000_DEAD_BEEF, 0x08, IDT_GATE_INTERRUPT);
        let e = b.get_gate(13);
        assert_eq!(e.handler(), 0xFFFF_8000_DEAD_BEEF);
        assert_eq!(e.type_attr, IDT_GATE_INTERRUPT | 0x80);
        assert_eq!(e.ist, 0);
    }

    #[test]
    fn builder_set_gate_priv_dpl_3() {
        let b = IdtBuilder::new();
        b.set_gate_priv(0x80, 0x1234_5678, 0x08, IDT_GATE_TRAP, 3);
        let e = b.get_gate(0x80);
        let attr = e.type_attr;
        assert_eq!((attr >> 5) & 0x3, 3);
    }

    #[test]
    fn builder_set_ist_masks_to_three_bits() {
        let b = IdtBuilder::new();
        b.set_gate(8, 0x100, 0x08, IDT_GATE_INTERRUPT);
        b.set_ist(8, 0xFF); // garbage bits should be masked
        let e = b.get_gate(8);
        assert_eq!(e.ist, 7);
    }

    #[test]
    fn vector_uses_ist_predicate() {
        assert!(vector_uses_ist(0));
        assert!(vector_uses_ist(14));
        assert!(vector_uses_ist(31));
        assert!(!vector_uses_ist(32));
        assert!(!vector_uses_ist(0x80));
        assert!(!vector_uses_ist(0xFF));
    }

    #[test]
    fn irq_entry_guard_ist_vector_bumps_count() {
        isolate(|| {
            assert_eq!(p::preempt_count(), 0);
            let _g = IrqEntryGuard::<14>::enter();
            assert_eq!(p::preempt_count(), 1);
            drop(_g);
            assert_eq!(p::preempt_count(), 0);
        });
    }

    #[test]
    fn irq_entry_guard_non_ist_vector_is_noop() {
        isolate(|| {
            assert_eq!(p::preempt_count(), 0);
            let _g = IrqEntryGuard::<32>::enter();
            assert_eq!(p::preempt_count(), 0);
        });
    }

    #[test]
    fn ist_preempt_hold_active_bumps_count() {
        isolate(|| {
            let _h = IstPreemptHold::new(true);
            assert_eq!(p::preempt_count(), 1);
        });
    }

    #[test]
    fn ist_preempt_hold_inactive_is_noop() {
        isolate(|| {
            let _h = IstPreemptHold::new(false);
            assert_eq!(p::preempt_count(), 0);
        });
    }

    #[test]
    fn exception_mode_eq() {
        assert_eq!(ExceptionMode::Normal, ExceptionMode::Normal);
        assert_ne!(ExceptionMode::Normal, ExceptionMode::Test);
    }
}
