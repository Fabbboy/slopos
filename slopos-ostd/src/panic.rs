//! Reliable Abort Core — the safe ostd surface for the fatal-fault / panic
//! path.
//!
//! All of the unsafe/asm machinery (the dual-stack emergency trampoline, the
//! per-CPU PCR slots) lives in `arch::x86_64::naked` / `cpu::x86_64::pcr`. This
//! module is the forbid-unsafe API that `boot`'s panic orchestration, the IDT
//! NMI handler, and the `mm` TLB shootdown consult — keeping the framekernel
//! discipline (no `unsafe` outside `slopos-ostd`) intact while still letting the
//! rest of the kernel drive single-owner election and the emergency stacks.
//!
//! Concurrency model (Linux `panic_cpu` cmpxchg + `crash_smp_send_stop`):
//! one CPU is elected the sole console driver; all others see `panic_owner_claimed`
//! and quietly stop. The owner NMI-broadcasts the stop, which also lets a wedged
//! `wait_for_acks` abandon its shootdown wait.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// Sentinel for "no CPU is driving a fatal panic".
const NO_OWNER: u32 = u32::MAX;

/// Count of peer CPUs that have acknowledged the panic-stop NMI (set by the IDT
/// NMI handler, read by the owner's bounded wait).
static STOPPED_CPUS: AtomicU32 = AtomicU32::new(0);

/// Record that this CPU has stopped in response to a panic-stop NMI.
#[inline]
pub fn mark_cpu_stopped() {
    STOPPED_CPUS.fetch_add(1, Ordering::SeqCst);
}

/// Number of peer CPUs that have acknowledged the panic stop.
#[inline]
pub fn stopped_cpu_count() -> u32 {
    STOPPED_CPUS.load(Ordering::Acquire)
}

/// Format a stashed `PanicInfo`'s location + message into `out`.
///
/// `info_ptr` is the panicking `&PanicInfo` re-narrowed to a raw pointer and
/// stashed before the emergency-stack switch; the switch only moves `RSP`, so
/// the `PanicInfo` (and the format args it borrows) remain live in memory. The
/// `unsafe` deref lives here so the (forbid-unsafe) `boot` reporter can format
/// the FULL message — including `panic!("{}", x)`-style formatted ones — with
/// the emergency data stack's headroom.
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

/// The single CPU elected to drive the fatal-panic console (Linux `panic_cpu`).
static PANIC_OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// Try to become the sole fatal-panic driver. Returns `true` iff THIS call won
/// the election (the owner was previously unclaimed). A loser must quietly
/// self-stop and never touch the console.
#[inline]
pub fn claim_panic_owner(cpu: u32) -> bool {
    PANIC_OWNER
        .compare_exchange(NO_OWNER, cpu, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// True once any CPU has claimed the fatal panic. Consulted by `wait_for_acks`
/// (abandon a shootdown wait) and the NMI handler (recognize a panic stop).
#[inline]
pub fn panic_owner_claimed() -> bool {
    PANIC_OWNER.load(Ordering::Acquire) != NO_OWNER
}

/// True if `cpu` is the elected fatal-panic owner — so a self-directed NMI does
/// not stop the owner that issued the broadcast.
#[inline]
pub fn panic_owner_is(cpu: u32) -> bool {
    PANIC_OWNER.load(Ordering::Acquire) == cpu
}

/// Test-only reset of the election state (no fatal panic actually occurred).
#[doc(hidden)]
#[inline]
pub fn reset_panic_owner_for_test() {
    PANIC_OWNER.store(NO_OWNER, Ordering::SeqCst);
}

/// Run the diverging fatal-fault reporter `f` on this CPU's emergency SAFE and
/// DATA stacks (see [`run_on_emergency_stacks`] doc for the dual-stack switch).
pub use crate::arch::x86_64::naked::run_on_emergency_stacks;

/// Enter the per-CPU fatal path, returning the previous recursion depth (a
/// non-zero value means the fatal path itself faulted → degrade to abort).
pub use crate::cpu::x86_64::pcr::panic_depth_enter;
pub use crate::cpu::x86_64::pcr::{
    in_interrupt_context, panic_in_flight_depth, panic_in_flight_enter, panic_in_flight_exit,
};

/// Test-mode QEMU-exit hook for [`abort_now`], registered by the boot crate
/// under its `tests` feature (ostd cannot depend on the test harness
/// directly). Left null in production builds.
pub type TestAbortShutdownFn = fn(i32);
static TEST_ABORT_SHUTDOWN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Register the test-mode shutdown hook `abort_now` calls before halting.
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

/// Guard for critical sections that must never be unwound through.
///
/// Dropping this guard during a panic unwind means a caller tried to recover
/// across a partially-mutated invariant boundary. That is not a task-scoped
/// oops; it is a kernel consistency failure, so the guard aborts.
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
