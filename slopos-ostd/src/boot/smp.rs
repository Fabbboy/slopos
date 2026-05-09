//! AP-bringup rendezvous.
//!
//! When the BSP fires up an AP via `limine::mp::Cpu::bootstrap`, the
//! AP enters [`crate::arch::x86_64::naked::ap_entry`] — a naked-asm
//! trampoline that installs `IA32_GS_BASE` from `AP_PCR_PTRS[slot - 1]`
//! and then **direct-jumps** (PC-relative, intra-OSTD) into the
//! non-naked, instrumentation-safe [`ap_early_entry`] below.
//!
//! `ap_early_entry` is deliberately minimal: it disables interrupts,
//! decodes the AP's 0-based `cpu_idx` from `MpInfo.extra`, and tail
//! calls a kernel-registered late-entry function pointer (provided
//! via [`register_ap_late_entry`] before any AP is fired).
//!
//! All architectural and policy setup (CR4 features, XSAVE, APIC
//! enable, IDT/IST, scheduler bringup) lives in the kernel-side late
//! entry. The split keeps OSTD free of `slopos-mm` / `slopos-drivers`
//! / `slopos-arch` runtime deps for AP startup.
//!
//! The cross-crate hand-off is a plain Rust function call from a stack
//! with valid GS_BASE, after the trampoline has fully returned to
//! instrumented Rust. There is **no asm tail-jump from naked code into
//! a cross-crate Rust symbol** — that pattern caused TCG-only crashes
//! in user-mode tests (`utest_fork`, `utest_io_capture`) the last
//! time it was tried.

use crate::cpu::x86_64::interrupts;
use crate::sync::OnceLock;

/// Kernel-supplied late-entry callback for AP bringup. Must be
/// registered via [`register_ap_late_entry`] **before** any AP is
/// fired by `limine::mp::Cpu::bootstrap`. Receives the AP's 0-based
/// `cpu_idx` (decoded from `MpInfo.extra - 1`).
pub static AP_LATE_ENTRY: OnceLock<fn(usize) -> !> = OnceLock::new();

/// Register the kernel-supplied AP late-entry callback. Single-writer:
/// call once on the BSP before bootstrapping any AP. Repeated calls
/// silently keep the first registration (`OnceLock` semantics).
pub fn register_ap_late_entry(entry: fn(usize) -> !) {
    AP_LATE_ENTRY.call_once(|| entry);
}

/// Offset of `MpInfo.extra_argument` in `limine::mp::MpInfo` (limine
/// 0.6.x). Pinned by Cargo.lock; a mismatched limine bump would
/// manifest as an immediate triple-fault on AP bringup.
const MP_INFO_EXTRA_OFFSET: usize = 24;

/// AP early entry — executes after [`ap_entry`] has installed
/// `IA32_GS_BASE`. Disables interrupts, decodes the AP's 0-based
/// `cpu_idx`, and tail-calls the kernel-registered late entry.
///
/// Takes `cpu_info` as `*const ()` to avoid a `limine` dep in OSTD;
/// the trampoline asm passes the same `rdi` value the bootloader
/// handed it.
///
/// [`ap_entry`]: crate::arch::x86_64::naked::ap_entry
///
/// # Safety
///
/// Caller must be a freshly-bootstrapped AP whose GS_BASE has just
/// been installed by the asm trampoline. `cpu_info` must be the
/// `MpInfo*` the bootloader handed to the trampoline. The
/// late-entry callback must already be registered (panics
/// otherwise via `OnceLock::wait`'s spin).
pub(crate) unsafe extern "C" fn ap_early_entry(cpu_info: *const ()) -> ! {
    interrupts::disable_interrupts();

    // Decode 1-based slot from `MpInfo.extra @ +24`, normalise to
    // 0-based.
    // SAFETY: trampoline passed us the bootloader-published MpInfo
    // pointer; the field at +24 is `AtomicU64` (8 bytes), readable
    // for as long as the bootloader keeps the response struct alive
    // (which is the kernel's lifetime).
    let slot_raw =
        unsafe { (cpu_info.cast::<u8>().add(MP_INFO_EXTRA_OFFSET) as *const u64).read() };
    let cpu_idx = slot_raw as usize;

    // Plain Rust function call from a fully-instrumented stack with
    // valid GS_BASE — no relocation hazards. Spins until the BSP
    // registers the kernel late entry; the BSP must call
    // `register_ap_late_entry(...)` before `cpu.bootstrap(...)`.
    let late = AP_LATE_ENTRY.wait();
    late(cpu_idx)
}
