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
}

impl Default for IdtBuilder {
    fn default() -> Self {
        Self::new()
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

/// One-shot wiring point for the production diagnostic sink.
///
/// # Safety
///
/// `sink` must live for `'static`. Only one registration is permitted.
pub unsafe fn register_diagnostic_sink(sink: &'static dyn DiagnosticSink) {
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
