//! Memory-ordering fence primitives.
//!
//! Wraps the unsafe [`core::arch::x86_64::_mm_sfence`] intrinsic in a
//! safe forwarder. SFENCE serializes prior store-class instructions
//! against subsequent stores and is needed after streaming or write-
//! combining stores (framebuffer blits, non-temporal stores) to make
//! them globally observable in program order.
//!
//! No memory aliasing or trap class concerns apply: SFENCE is a pure
//! ordering instruction and never accesses memory itself.

/// Emit an `SFENCE` instruction.
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
