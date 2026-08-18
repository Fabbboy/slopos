//! AP-bringup rendezvous.
//!
//! `limine::mp::Cpu::bootstrap` lands the AP in
//! [`crate::arch::x86_64::naked::ap_entry`], which installs `IA32_GS_BASE`
//! from `AP_PCR_PTRS[slot - 1]` and direct-jumps into [`ap_early_entry`];
//! that tail-calls the kernel-registered late entry, where all architectural
//! and policy setup lives. The split keeps OSTD free of `slopos-mm` /
//! `slopos-drivers` / `slopos-arch` runtime deps for AP startup.
//!
//! The cross-crate hand-off must stay a plain Rust call from instrumented
//! code: an asm tail-jump from naked code into a cross-crate Rust symbol
//! crashes under TCG.

use crate::cpu::x86_64::interrupts;
use crate::sync::{BspToken, OnceLock};

/// Kernel-supplied late-entry callback, registered via
/// [`register_ap_late_entry`] **before** any AP is fired. Receives the AP's
/// 1-based `cpu_idx`, which the BSP publishes in `MpInfo.extra`.
pub static AP_LATE_ENTRY: OnceLock<fn(usize) -> !> = OnceLock::new();

/// The `&BspToken<'brand>` witnesses BSP-only init; call once on the BSP
/// before bootstrapping any AP.
pub fn register_ap_late_entry<'brand>(_token: &BspToken<'brand>, entry: fn(usize) -> !) {
    AP_LATE_ENTRY.call_once(|| entry);
}

/// Offset of `MpInfo.extra_argument` in `limine::mp::MpInfo` (limine 0.6.x),
/// pinned by Cargo.lock; a mismatched bump triple-faults on AP bringup.
const MP_INFO_EXTRA_OFFSET: usize = 24;

/// Executes after [`ap_entry`] has installed `IA32_GS_BASE`. Takes `cpu_info`
/// as `*const ()` to avoid a `limine` dep in OSTD; the trampoline asm passes
/// the same `rdi` value the bootloader handed it.
///
/// [`ap_entry`]: crate::arch::x86_64::naked::ap_entry
///
/// # Safety
///
/// Caller must be a freshly-bootstrapped AP whose GS_BASE has just
/// been installed by the asm trampoline. `cpu_info` must be the
/// `MpInfo*` the bootloader handed to the trampoline. The late-entry
/// callback must already be registered: [`OnceLock::wait`] is a
/// blocking spin loop, so if [`register_ap_late_entry`] has not been
/// called by the BSP before the AP is bootstrapped this call will
/// **wedge the AP indefinitely** (unbounded busy-spin), not panic.
pub(crate) unsafe extern "C" fn ap_early_entry(cpu_info: *const ()) -> ! {
    interrupts::disable_interrupts();

    // The slot is 1-based end to end: the BSP writes `ap_slot` into this field
    // and the late entry consumes it as-is, so no `- 1` conversion happens.
    // SAFETY: trampoline passed us the bootloader-published MpInfo
    // pointer; the field at +24 is `AtomicU64` (8 bytes), readable
    // for as long as the bootloader keeps the response struct alive
    // (which is the kernel's lifetime).
    let slot_raw =
        unsafe { (cpu_info.cast::<u8>().add(MP_INFO_EXTRA_OFFSET) as *const u64).read() };
    let cpu_idx = slot_raw as usize;

    let late = AP_LATE_ENTRY.wait();
    late(cpu_idx)
}
