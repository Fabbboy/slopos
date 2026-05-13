//! Bootloader-published Higher-Half Direct Map (HHDM) offset.
//!
//! The bootloader (Limine) pre-maps all physical memory at
//! `KERNEL_VIRT_BASE + phys_offset`. Kernel-side `slopos-mm` owns the
//! authoritative copy of that offset, but the OSTD `boot::handoff`
//! primitives need to translate `PhysAddr → VirtAddr → &'static [u8]`
//! without taking a dep on `slopos-mm`. So OSTD exposes a one-shot
//! registry — the kernel publishes the offset once on BSP init through
//! a `BspToken` gate, and the handoff primitives read it.
//!
//! Mirrors the `register_*` style used elsewhere in OSTD
//! (`register_ap_late_entry`, `register_kernel_heap_backend`,
//! `register_send_ipi_to_cpu_fn`).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::{BspToken, InitFlag};

/// Sentinel — `0` means "not yet registered" because Limine never
/// publishes an HHDM at physical 0 (the bootloader reserves the
/// first MiB).
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// One-shot guard so a second BSP-init pathway can't silently overwrite
/// the offset.
static REGISTERED: InitFlag = InitFlag::new();

/// Publish the bootloader-supplied HHDM offset.
///
/// Single-writer: BSP-init only. The `BspToken` argument (minted by
/// [`crate::sync::run_bsp_init`]) ensures this can't be called outside
/// the one-shot init path. A second call panics so a regression in
/// the BSP-init protocol is caught loudly.
pub fn register_hhdm_offset<'brand>(_token: &BspToken<'brand>, offset: u64) {
    if !REGISTERED.init_once() {
        panic!("register_hhdm_offset called twice — BSP-init protocol regression");
    }
    HHDM_OFFSET.store(offset, Ordering::Release);
}

/// Read the registered HHDM offset.
///
/// Returns `None` if the offset has not yet been registered.
pub fn hhdm_offset() -> Option<u64> {
    if !REGISTERED.is_set() {
        return None;
    }
    Some(HHDM_OFFSET.load(Ordering::Acquire))
}

/// Test-only helper: re-arm the one-shot guard so an integration test
/// can mint a fresh HHDM-offset registration. Mirrors
/// `reset_bsp_token_for_tests` in `kernel_sync`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_hhdm_offset_for_tests() {
    HHDM_OFFSET.store(0, Ordering::Release);
    REGISTERED.reset();
}
