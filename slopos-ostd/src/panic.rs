//! Reliable Abort Core — the safe ostd surface for the fatal-fault / panic
//! path; the unsafe/asm machinery lives in `arch::x86_64::naked` and
//! `cpu::x86_64::pcr`.
//!
//! One CPU wins the owner CAS and becomes the sole console driver; all others
//! see `panic_owner_claimed` and quietly stop. The owner NMI-broadcasts the
//! stop, which also lets a wedged `wait_for_acks` abandon its shootdown wait.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

const NO_OWNER: u32 = u32::MAX;

/// Set by the IDT NMI handler, read by the owner's bounded wait.
static STOPPED_CPUS: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn mark_cpu_stopped() {
    STOPPED_CPUS.fetch_add(1, Ordering::SeqCst);
}

#[inline]
pub fn stopped_cpu_count() -> u32 {
    STOPPED_CPUS.load(Ordering::Acquire)
}

/// Set by any path that halts a CPU for good. Distinct from the oops counter,
/// which a *recovered* panic also bumps, so that counter cannot attribute a
/// latched lock-validator bypass to a fatal abort.
static FATAL_ABORTS: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn mark_fatal_abort() {
    FATAL_ABORTS.fetch_add(1, Ordering::SeqCst);
}

#[inline]
pub fn fatal_abort_observed() -> bool {
    FATAL_ABORTS.load(Ordering::Acquire) != 0
}

#[inline]
pub fn fatal_abort_count() -> u32 {
    FATAL_ABORTS.load(Ordering::Acquire)
}

/// Format a stashed `PanicInfo`'s location + message into `out`.
///
/// `info_ptr` is the panicking `&PanicInfo` stashed before the emergency-stack
/// switch; the switch only moves `RSP`, so the `PanicInfo` and the format args
/// it borrows remain live in memory.
pub fn format_panic_location_message(info_ptr: *const PanicInfo, out: &mut dyn Write) {
    if info_ptr.is_null() {
        let _ = out.write_str("<panic info unavailable>");
        return;
    }
    // SAFETY: the caller stashes a pointer to the live panicking `PanicInfo`;
    // the emergency-stack switch leaves that memory mapped and unmodified.
    let info = unsafe { &*info_ptr };
    if let Some(loc) = info.location() {
        let _ = write!(out, "{}:{}:{}: ", loc.file(), loc.line(), loc.column());
    }
    let _ = write!(out, "{}", info.message());
}

static PANIC_OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// Returns `true` iff THIS call won the election. A loser must quietly
/// self-stop and never touch the console.
#[inline]
pub fn claim_panic_owner(cpu: u32) -> bool {
    PANIC_OWNER
        .compare_exchange(NO_OWNER, cpu, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

#[inline]
pub fn panic_owner_claimed() -> bool {
    PANIC_OWNER.load(Ordering::Acquire) != NO_OWNER
}

/// True if `cpu` is the elected fatal-panic owner — a self-directed NMI must
/// not stop the owner that issued the broadcast.
#[inline]
pub fn panic_owner_is(cpu: u32) -> bool {
    PANIC_OWNER.load(Ordering::Acquire) == cpu
}

#[doc(hidden)]
#[inline]
pub fn reset_panic_owner_for_test() {
    PANIC_OWNER.store(NO_OWNER, Ordering::SeqCst);
}

pub use crate::arch::x86_64::naked::run_on_emergency_stacks;

pub use crate::cpu::x86_64::pcr::panic_depth_enter;
pub use crate::cpu::x86_64::pcr::{
    in_interrupt_context, panic_in_flight_depth, panic_in_flight_enter, panic_in_flight_exit,
};

/// Test-mode QEMU-exit hook for [`abort_now`], registered by the boot crate
/// because ostd cannot depend on the test harness. Null in production builds.
pub type TestAbortShutdownFn = fn(i32);
static TEST_ABORT_SHUTDOWN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_test_abort_shutdown(f: TestAbortShutdownFn) {
    TEST_ABORT_SHUTDOWN.store(f as *mut (), Ordering::Release);
}

/// Unconditional abort used when the unwinder catches a foreign exception at a
/// SlopOS-only boundary.
pub fn abort_now() -> ! {
    crate::cpu::x86_64::disable_interrupts();
    crate::early_console::write_bytes(b"\n\n=== KERNEL ABORT ===\n");
    crate::early_console::write_bytes(b"panic core abort\n");
    crate::early_console::write_bytes(b"System halted.\n");
    let shutdown = TEST_ABORT_SHUTDOWN.load(Ordering::Acquire);
    if !shutdown.is_null() {
        // SAFETY: stored only by `register_test_abort_shutdown` from a valid
        // `fn(i32)` pointer; function pointers are never deallocated.
        let shutdown: TestAbortShutdownFn = unsafe { core::mem::transmute(shutdown) };
        shutdown(1);
    }
    crate::cpu::x86_64::halt_loop()
}

/// Guard for critical sections that must never be unwound through: dropping it
/// mid-unwind is a kernel consistency failure, so it aborts.
///
/// Any section mutating a kernel-global multi-step invariant not restored by
/// `Drop`, and reachable from a recovery scope, must hold one. Arm it AFTER
/// acquiring the section's lock so drop order fires the abort before the lock
/// is released and a torn invariant is never republished. Every normal exit
/// MUST `disarm`: the in-flight depth stays non-zero for the whole panic
/// window, so a guard that normal-drops inside it would abort a healthy
/// section.
pub struct AbortOnUnwind {
    armed: bool,
}

impl AbortOnUnwind {
    #[inline]
    pub const fn new() -> Self {
        Self { armed: true }
    }

    #[inline]
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        if self.armed && panic_in_flight_depth() != 0 {
            abort_now();
        }
    }
}
