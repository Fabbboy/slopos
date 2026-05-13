//! Physical-to-virtual translation for the typed-frame layer.
//!
//! Holds a single kernel-wide HHDM-style offset that converts a
//! [`Paddr`] to the kernel virtual address where its page contents
//! live. This is the only path `UFrame`/`USegment` use to reach
//! frame contents for byte-copy I/O. The kernel boot path installs
//! the offset via [`init_phys_virt_offset`]; host-side integration
//! tests install it explicitly against scratch backing buffers.
//!
//! Kept independent of `slopos-mm::hhdm` — `slopos-ostd` must not
//! depend on `slopos-mm` (the dependency arrow points the other
//! way). The mm-side HHDM glue is expected to forward to
//! [`init_phys_virt_offset`] so both views agree on the offset.
//!
//! Atomic protocol: the offset is initialised exactly once via an
//! AcqRel `swap` against the [`UNINIT`] sentinel, mirroring the
//! one-shot pattern from [`crate::mm::frame::init_meta_slots`].
//! `slopos-sync::InitFlag` is deliberately not used — it would
//! invert the dependency graph.
//!
//! [`Paddr`]: crate::mm::frame::Paddr
//! [`UNINIT`]: self::UNINIT

use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::frame::Paddr;
use crate::sync::BspToken;

/// Sentinel for "phys-to-virt offset not yet installed". `u64::MAX`
/// is safe as a sentinel because no valid HHDM offset comes anywhere
/// near it on x86_64 (the canonical kernel half tops out at
/// `0xffff_ffff_ffff_ffff` only as a single point — practical HHDM
/// offsets sit in the `0xffff_8000_…` range).
const UNINIT: u64 = u64::MAX;

static PHYS_VIRT_OFFSET: AtomicU64 = AtomicU64::new(UNINIT);

/// One-shot wiring point. `offset` is the value to add to any
/// [`Paddr`] to reach a kernel virtual address mapping that page's
/// contents read+write. The `&BspToken<'brand>` witnesses BSP-only
/// init; `paddr.as_u64().wrapping_add(offset)` must be a valid kernel
/// virtual address mapped read+write for every `paddr` that will be
/// passed to a `UFrame`/`USegment` byte-copy method, and the mapping
/// must persist for the static lifetime of the kernel.
///
/// [`Paddr`]: crate::mm::frame::Paddr
pub fn init_phys_virt_offset<'brand>(_token: &BspToken<'brand>, offset: u64) {
    let prev = PHYS_VIRT_OFFSET.swap(offset, Ordering::AcqRel);
    assert_eq!(
        prev, UNINIT,
        "slopos_ostd::mm::phys::init_phys_virt_offset called twice"
    );
}

/// True once [`init_phys_virt_offset`] has been called.
#[inline]
pub fn is_initialised() -> bool {
    PHYS_VIRT_OFFSET.load(Ordering::Acquire) != UNINIT
}

/// Translate a [`Paddr`] to its kernel-virtual byte address.
///
/// Internal: callers (`UFrame`, `USegment`, future `IoMem`) must
/// guarantee that they only invoke this with frame paddrs they own,
/// so the resulting raw pointer is non-aliasing within their byte
/// window.
///
/// [`Paddr`]: crate::mm::frame::Paddr
#[inline]
pub(crate) fn phys_to_virt(paddr: Paddr) -> *mut u8 {
    let off = PHYS_VIRT_OFFSET.load(Ordering::Acquire);
    debug_assert_ne!(
        off, UNINIT,
        "slopos_ostd::mm::phys::phys_to_virt before init_phys_virt_offset"
    );
    paddr.as_u64().wrapping_add(off) as *mut u8
}

/// Test-only reset hook. Allows host integration-test binaries to
/// re-install a fresh offset between runs without re-loading the
/// crate. Not exposed in production builds.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    PHYS_VIRT_OFFSET.store(UNINIT, Ordering::Release);
}
