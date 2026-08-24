//! Interrupt flag management: sti, cli, irqsave/irqrestore.
//!
//! Off the kernel target (userland, host tests, Miri) a shadow stands in for
//! RFLAGS, so `IrqDisabled::with` still exercises its protocol there.
//!
//! Under `cfg(test)` that shadow is thread-local, because RFLAGS is per-CPU and
//! `cargo test` runs on parallel threads: one static lets one test's `cli` be
//! read as another's interrupt state.

#[allow(unused_imports)]
use core::arch::asm;

#[cfg(all(not(target_os = "none"), not(test)))]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(all(not(target_os = "none"), not(test)))]
static MOCK_RFLAGS: AtomicU64 = AtomicU64::new(1u64 << 9);

#[cfg(test)]
std::thread_local! {
    /// Starts with IF set, as a running CPU has it.
    static MOCK_RFLAGS: core::cell::Cell<u64> = const { core::cell::Cell::new(1u64 << 9) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
fn mock_rflags_get() -> u64 {
    #[cfg(test)]
    {
        MOCK_RFLAGS.with(|f| f.get())
    }
    #[cfg(not(test))]
    {
        MOCK_RFLAGS.load(Ordering::Relaxed)
    }
}

/// Returns the value from before the update.
#[cfg(not(target_os = "none"))]
#[inline(always)]
fn mock_rflags_update(set_if: bool) -> u64 {
    #[cfg(test)]
    {
        MOCK_RFLAGS.with(|f| {
            let prior = f.get();
            f.set(if set_if {
                prior | RFLAGS_IF
            } else {
                prior & !RFLAGS_IF
            });
            prior
        })
    }
    #[cfg(not(test))]
    {
        if set_if {
            MOCK_RFLAGS.fetch_or(RFLAGS_IF, Ordering::Relaxed)
        } else {
            MOCK_RFLAGS.fetch_and(!RFLAGS_IF, Ordering::Relaxed)
        }
    }
}

const RFLAGS_IF: u64 = 1u64 << 9;

#[inline(always)]
pub fn enable_interrupts() {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = mock_rflags_update(true);
    }
}

#[inline(always)]
pub fn disable_interrupts() {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = mock_rflags_update(false);
    }
}

/// Returns the RFLAGS value from before the `cli`.
#[inline(always)]
pub fn save_flags_cli() -> u64 {
    #[cfg(target_os = "none")]
    {
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
    #[cfg(not(target_os = "none"))]
    {
        mock_rflags_update(false)
    }
}

/// Re-enables interrupts only if IF was set in `flags`; no other flag is
/// written back.
#[inline(always)]
pub fn restore_flags(flags: u64) {
    if flags & RFLAGS_IF != 0 {
        enable_interrupts();
    }
}

/// Read RFLAGS without modifying interrupt state.
#[inline(always)]
pub fn read_rflags() -> u64 {
    #[cfg(target_os = "none")]
    {
        let flags: u64;
        unsafe {
            asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
        }
        flags
    }
    #[cfg(not(target_os = "none"))]
    {
        mock_rflags_get()
    }
}

#[inline(always)]
pub fn are_interrupts_enabled() -> bool {
    (read_rflags() & RFLAGS_IF) != 0
}

use core::marker::PhantomData;

/// Zero-sized capability witnessing that interrupts are disabled on the
/// current CPU for the duration of `'a`.
///
/// Constructed only by [`IrqDisabled::with`]. Functions that must run with
/// IRQs off take `&IrqDisabled<'_>`, so a block path that forgets `cli` does
/// not compile. `!Send + !Sync`: IRQ state is per-CPU.
#[derive(Debug)]
pub struct IrqDisabled<'a> {
    _scope: PhantomData<&'a ()>,
    _not_send: PhantomData<*const ()>,
}

impl IrqDisabled<'_> {
    /// Run `f` with IRQs disabled on this CPU, restoring the previous flag
    /// state afterwards. Re-entrant: nested calls compose, and IRQs come back
    /// on only if they were enabled at the outermost entry.
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
