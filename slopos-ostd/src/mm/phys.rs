//! Physical-to-virtual translation for the typed-frame layer.
//!
//! Holds one kernel-wide HHDM-style offset converting a frame `Paddr` to
//! the kernel virtual address of its contents, installed once by the boot
//! path. Kept independent of `slopos-mm::hhdm` because the dependency
//! arrow points the other way; the mm-side glue is expected to forward to
//! [`init_phys_virt_offset`] so both views agree.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::frame::Paddr;
use crate::sync::BspToken;

/// Sentinel for "offset not yet installed": practical HHDM offsets sit in
/// the `0xffff_8000_…` range, nowhere near `u64::MAX`.
const UNINIT: u64 = u64::MAX;

static PHYS_VIRT_OFFSET: AtomicU64 = AtomicU64::new(UNINIT);

/// One-shot wiring point witnessed by `BspToken`. For every `paddr` later
/// passed to a `UFrame`/`USegment` byte-copy method,
/// `paddr.as_u64().wrapping_add(offset)` must be a kernel virtual address
/// mapped read+write for the static lifetime of the kernel.
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

/// Callers must only pass frame paddrs they own, so the resulting pointer
/// is non-aliasing within their byte window.
#[inline]
pub(crate) fn phys_to_virt(paddr: Paddr) -> *mut u8 {
    let off = PHYS_VIRT_OFFSET.load(Ordering::Acquire);
    debug_assert_ne!(
        off, UNINIT,
        "slopos_ostd::mm::phys::phys_to_virt before init_phys_virt_offset"
    );
    // `with_exposed_provenance_mut` rather than `as *mut u8`: sound under
    // strict provenance, given the offset's installer already exposed the
    // backing allocation's provenance.
    let virt_addr = paddr.as_u64().wrapping_add(off) as usize;
    core::ptr::with_exposed_provenance_mut(virt_addr)
}

/// Lets a host integration-test binary re-install a fresh offset between
/// runs without re-loading the crate.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    PHYS_VIRT_OFFSET.store(UNINIT, Ordering::Release);
}
