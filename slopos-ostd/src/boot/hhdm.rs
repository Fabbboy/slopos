//! Bootloader-published Higher-Half Direct Map (HHDM) offset.
//!
//! Kernel-side `slopos-mm` owns the authoritative copy, but the OSTD
//! `boot::handoff` primitives must translate `PhysAddr → VirtAddr` without
//! taking a dep on it, so the kernel publishes the offset here once on BSP
//! init through a `BspToken` gate.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::{BspToken, InitFlag};

/// `0` means "not yet registered": Limine never publishes an HHDM at physical
/// 0, having reserved the first MiB.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// One-shot guard so a second BSP-init pathway can't silently overwrite
/// the offset.
static REGISTERED: InitFlag = InitFlag::new();

/// Publish the bootloader-supplied HHDM offset.
///
/// Single-writer, BSP-init only; a second call panics rather than silently
/// replacing the offset.
pub fn register_hhdm_offset<'brand>(_token: &BspToken<'brand>, offset: u64) {
    if !REGISTERED.init_once() {
        panic!("register_hhdm_offset called twice — BSP-init protocol regression");
    }
    HHDM_OFFSET.store(offset, Ordering::Release);
}

/// Read the registered HHDM offset; `None` before registration.
pub fn hhdm_offset() -> Option<u64> {
    if !REGISTERED.is_set() {
        return None;
    }
    Some(HHDM_OFFSET.load(Ordering::Acquire))
}

/// Test-only: re-arm the one-shot guard so a test can register a fresh offset.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_hhdm_offset_for_tests() {
    HHDM_OFFSET.store(0, Ordering::Release);
    REGISTERED.reset();
}
