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

/// x87 FPU Control Word offset within both the FXSAVE image and the XSAVE
/// legacy region.
pub(crate) const LEGACY_FCW_OFFSET: usize = 0;

/// MXCSR offset within both the FXSAVE image and the XSAVE legacy region.
pub(crate) const LEGACY_MXCSR_OFFSET: usize = 24;

/// Offset of the XSTATE header, which follows the 512-byte legacy region.
const XSTATE_HEADER_OFFSET: usize = 512;

/// Offset of `XSTATE_BV`: the bitmap of components present in the image.
pub const XSTATE_BV_OFFSET: usize = XSTATE_HEADER_OFFSET;

/// Offset of `XCOMP_BV`. Bit 63 selects the compacted format; the low bits
/// then enumerate the components in it.
pub const XCOMP_BV_OFFSET: usize = XSTATE_HEADER_OFFSET + 8;

/// Offset of the XSTATE header's reserved tail, which must read as zero.
pub const XSTATE_RESERVED_OFFSET: usize = XSTATE_HEADER_OFFSET + 16;

/// Length of that reserved tail — the header is 64 bytes in total.
const XSTATE_RESERVED_LEN: usize = 48;

/// FPU/SIMD state save area.
///
/// Sized to [`FPU_STATE_SIZE`] and 64-byte aligned per the XSAVE/XRSTOR
/// hardware requirement.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct FpuState {
    pub data: [u8; FPU_STATE_SIZE],
}

// FPU_STATE_SIZE is a multiple of the 64-byte alignment, so the shell
// adds no tail padding and the struct size equals the constant that
// stack/heap layouts are budgeted against.
const _: () = assert!(core::mem::size_of::<FpuState>() == FPU_STATE_SIZE);

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
        let mut state = Self::zero();
        // FCW = 0x037F (all x87 exceptions masked).
        state.data[LEGACY_FCW_OFFSET] = 0x7F;
        state.data[LEGACY_FCW_OFFSET + 1] = 0x03;
        // MXCSR = 0x1F80 (all SSE exceptions masked).
        state.data[LEGACY_MXCSR_OFFSET] = 0x80;
        state.data[LEGACY_MXCSR_OFFSET + 1] = 0x1F;
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
                bytes.add(LEGACY_FCW_OFFSET).write(0x7F);
                bytes.add(LEGACY_FCW_OFFSET + 1).write(0x03);
                // MXCSR = 0x1F80.
                bytes.add(LEGACY_MXCSR_OFFSET).write(0x80);
                bytes.add(LEGACY_MXCSR_OFFSET + 1).write(0x1F);
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

// ---------------------------------------------------------------------------
// Image validation
// ---------------------------------------------------------------------------

/// Why an XSAVE image would fault if handed to `XRSTOR64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XsaveImageError {
    /// `XSTATE_BV` names a component the active XCR0 does not enable.
    XstateBvOutsideXcr0,
    /// `XCOMP_BV` is non-zero, i.e. the image claims the compacted format.
    Compacted,
    /// The XSTATE header's reserved tail is not zero.
    ReservedHeader,
    /// `MXCSR` carries a bit this CPU does not implement.
    ReservedMxcsr,
}

#[inline]
fn read_u64(bytes: &[u8; FPU_STATE_SIZE], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

/// Check that `bytes` is an XSAVE image `XRSTOR64` will accept.
///
/// The kernel produces and consumes only the standard, non-compacted format,
/// which is what makes these four conditions exhaustive for a buffer of this
/// fixed size. `xcr0` and `mxcsr_mask` are parameters rather than global reads
/// so the rules stand on their own and can be exercised against a CPU other
/// than the one running.
pub fn validate_xsave_image(
    bytes: &[u8; FPU_STATE_SIZE],
    xcr0: u64,
    mxcsr_mask: u32,
) -> Result<(), XsaveImageError> {
    if read_u64(bytes, XSTATE_BV_OFFSET) & !xcr0 != 0 {
        return Err(XsaveImageError::XstateBvOutsideXcr0);
    }

    // Bit 63 selects the compacted format and the low bits enumerate its
    // components; neither is something this kernel ever writes, and XRSTOR64
    // faults on either.
    if read_u64(bytes, XCOMP_BV_OFFSET) != 0 {
        return Err(XsaveImageError::Compacted);
    }

    let reserved = &bytes[XSTATE_RESERVED_OFFSET..XSTATE_RESERVED_OFFSET + XSTATE_RESERVED_LEN];
    if reserved.iter().any(|&b| b != 0) {
        return Err(XsaveImageError::ReservedHeader);
    }

    // The one that is easy to miss: MXCSR is restored from the legacy region
    // whether or not XSTATE_BV claims the SSE component, and a reserved bit
    // there is a #GP just the same.
    let mxcsr = u32::from_le_bytes([
        bytes[LEGACY_MXCSR_OFFSET],
        bytes[LEGACY_MXCSR_OFFSET + 1],
        bytes[LEGACY_MXCSR_OFFSET + 2],
        bytes[LEGACY_MXCSR_OFFSET + 3],
    ]);
    if mxcsr & !mxcsr_mask != 0 {
        return Err(XsaveImageError::ReservedMxcsr);
    }

    Ok(())
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

// ---------------------------------------------------------------------------
// Fault-recoverable XRSTOR64.
//
// `XRSTOR64` faults on an image the hardware objects to, and the only image
// the kernel does not author itself is the one `rt_sigreturn` accepts from the
// user stack. Validation rejects the conditions the SDM documents, but a ring-0
// #GP has no unwind path — the dispatcher bumps interrupt nesting before any
// handler runs, and the panic handler refuses to unwind from there — so an
// un-modelled condition, or a new XCR0 component, would halt the machine
// rather than fail a syscall.
//
// Routing the instruction through a known RIP range makes that survivable: the
// #GP handler recognises a kernel-mode fault inside
// `__ostd_fpu_xrstor_start..end` and redirects RIP to the failure tail. Both
// production XRSTOR sites — sigreturn and the context switch — go through this
// band, so neither can be the one that stops the CPU.
// ---------------------------------------------------------------------------

core::arch::global_asm!(
    ".global __ostd_fpu_xrstor",
    ".global __ostd_fpu_xrstor_start",
    ".global __ostd_fpu_xrstor_end",
    ".global __ostd_fpu_xrstor_fault",
    "__ostd_fpu_xrstor:",
    "    mov eax, esi",
    "    mov rdx, rsi",
    "    shr rdx, 32",
    "__ostd_fpu_xrstor_start:",
    "    xrstor64 [rdi]",
    "__ostd_fpu_xrstor_end:",
    "    mov eax, 1",
    "    ret",
    "__ostd_fpu_xrstor_fault:",
    "    xor eax, eax",
    "    ret",
);

unsafe extern "C" {
    fn __ostd_fpu_xrstor(state: *const FpuState, xcr0_mask: u64) -> u64;
    fn __ostd_fpu_xrstor_start();
    fn __ostd_fpu_xrstor_end();
    fn __ostd_fpu_xrstor_fault();
}

/// Returns `true` if `rip` falls within the XRSTOR64 fault region. The
/// general-protection handler queries this on kernel-mode faults and redirects
/// RIP to [`fpu_xrstor_fault_ip`] on a match.
#[inline]
pub fn is_fpu_xrstor_ip(rip: u64) -> bool {
    let start = __ostd_fpu_xrstor_start as *const () as u64;
    let end = __ostd_fpu_xrstor_end as *const () as u64;
    rip >= start && rip < end
}

/// RIP the general-protection handler rewrites to when [`is_fpu_xrstor_ip`]
/// matches — the restore's failure tail.
#[inline]
pub fn fpu_xrstor_fault_ip() -> u64 {
    __ostd_fpu_xrstor_fault as *const () as u64
}

/// Restore the CPU's FPU/SIMD state from `state` using XRSTOR64.
///
/// Returns `false` if the hardware rejected the image, in which case the
/// register file holds whatever the partial restore left and the caller must
/// define it — [`validate_xsave_image`] is the check that keeps this from
/// happening for a user-supplied image in the first place.
///
/// # Safety
///
/// Same as [`fpu_xsave`] — `state` must be valid + 64-byte aligned and
/// not concurrently mutated.
#[must_use]
#[inline]
pub unsafe fn fpu_xrstor(state: *const FpuState, xcr0_mask: u64) -> bool {
    // SAFETY: caller certifies `state` is properly aligned + readable; a #GP
    // inside the band lands on the failure tail rather than the panic path.
    unsafe { __ostd_fpu_xrstor(state, xcr0_mask) != 0 }
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

    /// Safe wrapper around [`fpu_xrstor`], with its `false`-on-rejection
    /// return.
    ///
    /// `&self` plus the scheduler's IRQs-off + on-this-CPU invariant
    /// discharge the safety contract. `&self` is sound here because
    /// `XRSTOR64` only reads the buffer.
    #[must_use]
    #[inline]
    pub fn restore_to_cpu(&self, xcr0_mask: u64) -> bool {
        // SAFETY: `&self` keeps the buffer borrowed read-only; XRSTOR64
        // only reads. Alignment is guaranteed by `#[repr(C, align(64))]`.
        unsafe { fpu_xrstor(self as *const FpuState, xcr0_mask) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fpu_state_is_64_byte_aligned() {
        assert_eq!(core::mem::align_of::<FpuState>(), 64);
    }

    // `size_of::<FpuState>() == FPU_STATE_SIZE` is a `const _` assert
    // beside the struct definition; no runtime duplicate here.

    #[test]
    fn fpu_state_new_sets_fcw_and_mxcsr() {
        let s = FpuState::new();
        assert_eq!(s.data[0], 0x7F);
        assert_eq!(s.data[1], 0x03);
        assert_eq!(s.data[24], 0x80);
        assert_eq!(s.data[25], 0x1F);
    }

    // X87 | SSE | AVX — what the kernel enables on any machine with AVX.
    const TEST_XCR0: u64 = 0b111;
    // What every CPU that implements DAZ reports.
    const TEST_MXCSR_MASK: u32 = 0xFFFF;

    fn write_u64(state: &mut FpuState, offset: usize, value: u64) {
        state.data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn validate(state: &FpuState) -> Result<(), XsaveImageError> {
        validate_xsave_image(&state.data, TEST_XCR0, TEST_MXCSR_MASK)
    }

    #[test]
    fn validate_accepts_the_kernel_default_and_all_zero_images() {
        assert_eq!(validate(&FpuState::new()), Ok(()));
        assert_eq!(validate(&FpuState::zero()), Ok(()));
    }

    #[test]
    fn validate_accepts_xstate_bv_within_xcr0() {
        let mut state = FpuState::new();
        write_u64(&mut state, XSTATE_BV_OFFSET, TEST_XCR0);
        assert_eq!(validate(&state), Ok(()));
    }

    #[test]
    fn validate_rejects_xstate_bv_outside_xcr0() {
        // Bit 3 is BNDREGS, which this kernel never enables in XCR0.
        let mut state = FpuState::new();
        write_u64(&mut state, XSTATE_BV_OFFSET, TEST_XCR0 | (1 << 3));
        assert_eq!(validate(&state), Err(XsaveImageError::XstateBvOutsideXcr0));
    }

    #[test]
    fn validate_rejects_compacted_format_bit() {
        let mut state = FpuState::new();
        write_u64(&mut state, XCOMP_BV_OFFSET, 1 << 63);
        assert_eq!(validate(&state), Err(XsaveImageError::Compacted));
    }

    #[test]
    fn validate_rejects_xcomp_bv_without_the_format_bit() {
        // A component bit with bit 63 clear is not a compacted image, but it is
        // not something XRSTOR64 accepts either.
        let mut state = FpuState::new();
        write_u64(&mut state, XCOMP_BV_OFFSET, 1);
        assert_eq!(validate(&state), Err(XsaveImageError::Compacted));
    }

    #[test]
    fn validate_rejects_any_dirty_reserved_header_byte() {
        for i in 0..XSTATE_RESERVED_LEN {
            let mut state = FpuState::new();
            state.data[XSTATE_RESERVED_OFFSET + i] = 1;
            assert_eq!(
                validate(&state),
                Err(XsaveImageError::ReservedHeader),
                "reserved header byte {i} was accepted"
            );
        }
    }

    #[test]
    fn validate_rejects_reserved_mxcsr_bits() {
        let mut state = FpuState::new();
        let mxcsr = MXCSR_DEFAULT | !TEST_MXCSR_MASK;
        state.data[LEGACY_MXCSR_OFFSET..LEGACY_MXCSR_OFFSET + 4]
            .copy_from_slice(&mxcsr.to_le_bytes());
        assert_eq!(validate(&state), Err(XsaveImageError::ReservedMxcsr));
    }

    #[test]
    fn validate_accepts_daz_when_the_mask_allows_it() {
        // The regression guard for hardcoding the mask to 0xFFBF, which clears
        // bit 6 and would reject a legitimate denormals-are-zero setting.
        let mut state = FpuState::new();
        let mxcsr = MXCSR_DEFAULT | (1 << 6);
        state.data[LEGACY_MXCSR_OFFSET..LEGACY_MXCSR_OFFSET + 4]
            .copy_from_slice(&mxcsr.to_le_bytes());
        assert_eq!(validate(&state), Ok(()));
        assert_eq!(
            validate_xsave_image(&state.data, TEST_XCR0, 0xFFBF),
            Err(XsaveImageError::ReservedMxcsr)
        );
    }

    #[test]
    fn validate_ignores_the_legacy_region_beyond_mxcsr() {
        // The XMM/x87 payload is data, not metadata: XRSTOR64 loads whatever is
        // there. Rejecting it would break every real signal return.
        let mut state = FpuState::new();
        for byte in state.data[32..XSTATE_HEADER_OFFSET].iter_mut() {
            *byte = 0xFF;
        }
        assert_eq!(validate(&state), Ok(()));
    }

    // The next three depend on the real binary layout of the `global_asm!`
    // block (`__ostd_fpu_xrstor_start` immediately precedes the `xrstor64`
    // byte, `__ostd_fpu_xrstor_end` follows it, and the fault tail sits past
    // the `ret`). Miri assigns external-fn pointer values that do not preserve
    // that layout, so the start..end range collapses and the checks become
    // meaningless. Skip under Miri.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn xrstor_fault_ip_is_outside_the_recoverable_range() {
        let fault = fpu_xrstor_fault_ip();
        assert!(fault != 0);
        assert!(!is_fpu_xrstor_ip(fault));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn xrstor_range_contains_the_instruction() {
        let start = __ostd_fpu_xrstor_start as *const () as u64;
        assert!(is_fpu_xrstor_ip(start));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn xrstor_range_excludes_addresses_below_start() {
        let start = __ostd_fpu_xrstor_start as *const () as u64;
        assert!(!is_fpu_xrstor_ip(start - 1));
    }
}
