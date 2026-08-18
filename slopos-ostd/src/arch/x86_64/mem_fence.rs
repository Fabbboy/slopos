//! Memory-ordering fence primitives. SFENCE is needed after write-combining or
//! non-temporal stores (framebuffer blits) to make them globally observable in
//! program order.

#[inline]
pub fn sfence() {
    // SAFETY: SFENCE is a pure CPU ordering primitive — it does not
    // access memory and has no trap class. The intrinsic is `unsafe`
    // only because `core::arch::*` historically gates all SIMD-style
    // intrinsics behind `unsafe`; no caller-side invariant is needed.
    unsafe {
        core::arch::x86_64::_mm_sfence();
    }
}
