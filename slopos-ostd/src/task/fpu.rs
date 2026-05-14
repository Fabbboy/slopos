//! FPU/SIMD state save/restore via XSAVE64 / XRSTOR64.
//!
//! [`FpuState`] is the 64-byte-aligned XSAVE area used by every kernel
//! task. Its size is the compile-time maximum across current x86-64
//! XSAVE features (FXSAVE = 512 B, AVX = 832 B, AVX-512 = 2,688 B) so a
//! single layout serves every CPU; runtime negotiation via
//! `crate::cpu::x86_64::xsave::active_xcr0()` controls which components
//! the hardware actually touches.

use core::arch::asm;

use crate::mm::AllocError;
use crate::mm::init::{Init, Zeroable, init_from_closure};

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

// SAFETY: `FpuState` is `[u8; FPU_STATE_SIZE]` wrapped in a 64-byte
// alignment shell. The all-zero pattern matches `FpuState::zero()` —
// XRSTOR with that buffer treats every component header (XSTATE_BV,
// XCOMP_BV) as zero and uses processor-reset defaults. The legacy x87
// FCW / MXCSR are zero too, which is technically a non-default
// (FCW=0x037F, MXCSR=0x1F80 are the kernel defaults), but the zero
// pattern is still a representationally valid `FpuState` — anyone
// requiring the kernel-default mask should call `init_default()`.
unsafe impl Zeroable for FpuState {}

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

    /// In-place [`Init`] recipe equivalent to [`Self::new`]. Used by
    /// `KBox::try_init(FpuState::init_default())` so runtime callers
    /// don't materialise a 2.6 KiB rvalue on their own stack frame.
    ///
    /// The `E = AllocError` parameter is purely an absorption shim so
    /// the recipe slots into `KBox::try_init`'s `E: From<AllocError>`
    /// bound — the closure itself never errors.
    pub fn init_default() -> impl Init<Self, AllocError> {
        // SAFETY: zeroes the slot, then writes the legacy FCW (0x037F)
        // and MXCSR (0x1F80) so the result is byte-for-byte identical
        // to `Self::new()`. Returns `Ok(())` only after all writes.
        unsafe {
            init_from_closure(|slot: *mut Self| -> Result<(), AllocError> {
                let bytes = slot as *mut u8;
                core::ptr::write_bytes(bytes, 0, core::mem::size_of::<Self>());
                // FCW = 0x037F.
                bytes.add(0).write(0x7F);
                bytes.add(1).write(0x03);
                // MXCSR = 0x1F80.
                bytes.add(24).write(0x80);
                bytes.add(25).write(0x1F);
                Ok(())
            })
        }
    }

    /// In-place [`Init`] recipe equivalent to [`Self::zero`]. Trivial
    /// counterpart to [`Self::init_default`] when the caller needs an
    /// XSTATE-untouched buffer with no FCW/MXCSR seed bits. See
    /// [`Self::init_default`] for the `AllocError` rationale.
    pub fn init_zero() -> impl Init<Self, AllocError> {
        // SAFETY: writes `size_of::<Self>()` zero bytes into `slot`,
        // matching `Self::zero()` byte for byte. `FpuState: Zeroable`
        // certifies the zero pattern is a valid `Self`.
        unsafe {
            init_from_closure(|slot: *mut Self| -> Result<(), AllocError> {
                core::ptr::write_bytes(slot as *mut u8, 0, core::mem::size_of::<Self>());
                Ok(())
            })
        }
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

impl FpuState {
    /// Safe wrapper around [`fpu_xsave`].
    ///
    /// `&mut self` discharges the exclusive-write half of the contract.
    /// The remaining "interrupts disabled / scheduler serialisation"
    /// requirement is the standard context-switch invariant the
    /// scheduler upholds at every call site — kept implicit because
    /// every consumer of this API operates inside that window.
    #[inline]
    pub fn save_current(&mut self, xcr0_mask: u64) {
        // SAFETY: `&mut self` guarantees exclusive access + 64-byte
        // alignment (the type is `#[repr(C, align(64))]`). Scheduler
        // callers run with IRQs disabled.
        unsafe { fpu_xsave(self as *mut FpuState, xcr0_mask) };
    }

    /// Safe wrapper around [`fpu_xrstor`].
    ///
    /// `&self` plus the scheduler's IRQs-off + on-this-CPU invariant
    /// discharge the safety contract. `&self` is sound here because
    /// `XRSTOR64` only reads the buffer.
    #[inline]
    pub fn restore_to_cpu(&self, xcr0_mask: u64) {
        // SAFETY: `&self` keeps the buffer borrowed read-only; XRSTOR64
        // only reads. Alignment is guaranteed by `#[repr(C, align(64))]`.
        unsafe { fpu_xrstor(self as *const FpuState, xcr0_mask) };
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
