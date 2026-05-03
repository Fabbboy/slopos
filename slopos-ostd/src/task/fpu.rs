//! FPU/SIMD state save/restore via XSAVE64 / XRSTOR64.
//!
//! [`FpuState`] is the 64-byte-aligned XSAVE area used by every kernel
//! task. Its size is the compile-time maximum across current x86-64
//! XSAVE features (FXSAVE = 512 B, AVX = 832 B, AVX-512 = 2,688 B) so a
//! single layout serves every CPU; runtime negotiation via
//! `crate::cpu::x86_64::xsave::active_xcr0()` controls which components
//! the hardware actually touches.

use core::arch::asm;

/// Size of the per-task FPU state save area.
///
/// Sized to the AVX-512 worst case (2,688 bytes). FXSAVE-only CPUs use
/// the first 512 bytes; the remaining bytes are unused but reserved.
pub const FPU_STATE_SIZE: usize = 2688;

/// Legacy FXSAVE area size (512 B). Used as the fallback when XSAVE is
/// not available.
pub const FXSAVE_AREA_SIZE: usize = 512;

/// Default MXCSR value: all SSE exceptions masked.
pub const MXCSR_DEFAULT: u32 = 0x1F80;

/// FPU/SIMD state save area.
///
/// Sized to [`FPU_STATE_SIZE`] and 64-byte aligned per the XSAVE/XRSTOR
/// hardware requirement.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct FpuState {
    pub data: [u8; FPU_STATE_SIZE],
}

impl FpuState {
    /// All-zeroes save area.
    pub const fn zero() -> Self {
        Self {
            data: [0u8; FPU_STATE_SIZE],
        }
    }

    /// Default FPU state with x87/SSE exceptions masked and XSAVE header
    /// zeroed (XSTATE_BV = 0, XCOMP_BV = 0 — hardware uses processor-reset
    /// defaults for every component on the next XRSTOR).
    pub const fn new() -> Self {
        // x87 FPU Control Word offset within both FXSAVE and XSAVE legacy region.
        const FCW_OFFSET: usize = 0;
        // MXCSR offset within both FXSAVE and XSAVE legacy region.
        const MXCSR_OFFSET: usize = 24;

        let mut state = Self::zero();
        // FCW = 0x037F (all x87 exceptions masked).
        state.data[FCW_OFFSET] = 0x7F;
        state.data[FCW_OFFSET + 1] = 0x03;
        // MXCSR = 0x1F80 (all SSE exceptions masked).
        state.data[MXCSR_OFFSET] = 0x80;
        state.data[MXCSR_OFFSET + 1] = 0x1F;
        state
    }
}

impl Default for FpuState {
    fn default() -> Self {
        Self::new()
    }
}

/// Save the current CPU's FPU/SIMD state into `state` using XSAVE64.
///
/// # Safety
///
/// - `state` must point to a valid, 64-byte-aligned [`FpuState`].
/// - Must be called with no concurrent write to that buffer (Inv. 8 — a
///   given `Task` is on at most one CPU at a time).
/// - Must be called with interrupts disabled or under whatever
///   serialisation the scheduler provides.
#[inline]
pub unsafe fn fpu_xsave(state: *mut FpuState, xcr0_mask: u64) {
    let lo = xcr0_mask as u32;
    let hi = (xcr0_mask >> 32) as u32;
    // SAFETY: caller certifies `state` is properly aligned + exclusive.
    unsafe {
        asm!(
            "xsave64 [{}]",
            in(reg) state,
            in("eax") lo,
            in("edx") hi,
            options(nostack),
        );
    }
}

/// Restore the CPU's FPU/SIMD state from `state` using XRSTOR64.
///
/// # Safety
///
/// Same as [`fpu_xsave`] — `state` must be valid + 64-byte aligned and
/// not concurrently mutated.
#[inline]
pub unsafe fn fpu_xrstor(state: *const FpuState, xcr0_mask: u64) {
    let lo = xcr0_mask as u32;
    let hi = (xcr0_mask >> 32) as u32;
    // SAFETY: caller certifies `state` is properly aligned + readable.
    unsafe {
        asm!(
            "xrstor64 [{}]",
            in(reg) state,
            in("eax") lo,
            in("edx") hi,
            clobber_abi("sysv64"),
            options(nostack, readonly),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fpu_state_is_64_byte_aligned() {
        assert_eq!(core::mem::align_of::<FpuState>(), 64);
    }

    #[test]
    fn fpu_state_size_matches_constant() {
        assert_eq!(core::mem::size_of::<FpuState>(), FPU_STATE_SIZE);
    }

    #[test]
    fn fpu_state_new_sets_fcw_and_mxcsr() {
        let s = FpuState::new();
        assert_eq!(s.data[0], 0x7F);
        assert_eq!(s.data[1], 0x03);
        assert_eq!(s.data[24], 0x80);
        assert_eq!(s.data[25], 0x1F);
    }
}
