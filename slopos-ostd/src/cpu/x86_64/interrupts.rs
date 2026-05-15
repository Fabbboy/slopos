//! Interrupt flag management: sti, cli, irqsave/irqrestore.

use core::arch::asm;

/// Enable interrupts (STI).
#[inline(always)]
pub fn enable_interrupts() {
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

/// Disable interrupts (CLI).
#[inline(always)]
pub fn disable_interrupts() {
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

/// Save RFLAGS and disable interrupts (irqsave pattern).
/// Returns the saved RFLAGS value.
#[inline(always)]
pub fn save_flags_cli() -> u64 {
    let flags: u64;
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) flags,
            options(nomem)
        );
    }
    flags
}

/// Restore interrupt flag from saved RFLAGS (irqrestore pattern).
/// Only re-enables interrupts if they were enabled in the saved flags.
#[inline(always)]
pub fn restore_flags(flags: u64) {
    // Check if IF (bit 9) was set in the saved flags
    if flags & (1 << 9) != 0 {
        enable_interrupts();
    }
}

/// Read RFLAGS register without modifying interrupt state.
/// Use `save_flags_cli()` if you need to disable interrupts atomically.
#[inline(always)]
pub fn read_rflags() -> u64 {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    flags
}

/// Returns true if interrupts are currently enabled (IF bit set).
#[inline(always)]
pub fn are_interrupts_enabled() -> bool {
    (read_rflags() & (1 << 9)) != 0
}

// ---------------------------------------------------------------------------
// IrqDisabled<'a> — lifetime-scoped capability proving IRQs are off
// ---------------------------------------------------------------------------

use core::marker::PhantomData;

/// Zero-sized capability witnessing that interrupts are disabled on
/// the current CPU for the duration of `'a`.
///
/// Constructed only by [`IrqDisabled::with`], which `cli`/`sti`-wraps
/// the supplied closure and hands the closure a borrowed token whose
/// lifetime is bounded by the call scope. Functions that must run
/// with IRQs off (e.g. `yield_blocked_task` in the scheduler) take
/// `&IrqDisabled<'_>` to push the discipline into the type system —
/// a future block path that forgets `cli` will not compile.
///
/// Bug history motivating this capability: the timer ISR firing
/// between the Running→Blocked CAS and the `schedule()` yield
/// could hand a wake to the still-currently-executing task on the
/// same CPU, racing with the in-progress block sequence. The
/// `cli`-around-the-window fix is correct but discipline-by-comment;
/// `IrqDisabled<'cli>` makes the cli scope a compile-time
/// requirement.
///
/// The token is `!Send + !Sync` (via `PhantomData<*const ()>`) so it
/// cannot leak across CPUs — IRQ state is per-CPU.
#[derive(Debug)]
pub struct IrqDisabled<'a> {
    _scope: PhantomData<&'a ()>,
    _not_send: PhantomData<*const ()>,
}

impl IrqDisabled<'_> {
    /// Run `f` with IRQs disabled on this CPU. The closure receives
    /// a borrowed [`IrqDisabled`] token whose lifetime is bounded by
    /// the call. After `f` returns, the previous IRQ-flag state is
    /// restored.
    ///
    /// Re-entrant: nested `with(...)` calls compose; the innermost
    /// `restore_flags` only re-enables IRQs if they were enabled at
    /// the outermost entry.
    #[inline]
    pub fn with<R>(f: impl for<'a> FnOnce(&'a IrqDisabled<'a>) -> R) -> R {
        let saved = save_flags_cli();
        let token = IrqDisabled {
            _scope: PhantomData,
            _not_send: PhantomData,
        };
        let r = f(&token);
        restore_flags(saved);
        r
    }
}
