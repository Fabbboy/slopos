//! XSAVE / XRSTOR initialisation and runtime queries.
//!
//! XSAVE is a hard boot requirement: `init()` panics if the CPU lacks it and
//! there is no FXSAVE fallback. The BSP runs `init()` before SMP; each AP then
//! calls `enable_on_current_cpu()` to replicate the CR4 + XCR0 configuration.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::control_regs::{Osxsave, Xcr0Flags, Xcr0Mask, xcr0_write};
use crate::arch::x86_64::cpuid::XsaveFeatures;
use crate::task::FXSAVE_AREA_SIZE;

/// Active XSAVE area size in bytes; 0 until `init()` runs.
static XSAVE_AREA_SIZE: AtomicUsize = AtomicUsize::new(0);

/// XCR0 value computed by the BSP — every AP writes the same mask.
static ACTIVE_XCR0: AtomicU64 = AtomicU64::new(0);

/// `true` when `XSAVEC` is available (compact save format, no gaps).
static XSAVEC_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// `true` when `XSAVEOPT` is available (optimised — only writes dirty state).
static XSAVEOPT_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// The set of MXCSR bits this CPU implements. Loading a bit outside it raises
/// `#GP`, which makes this the authority for validating an MXCSR word that
/// came from user memory. Seeded with [`MXCSR_MASK_DEFAULT`] rather than zero
/// so a read before `init()` rejects reserved bits instead of everything.
static MXCSR_FEATURE_MASK: AtomicU32 = AtomicU32::new(MXCSR_MASK_DEFAULT);

/// The MXCSR mask to assume when the FXSAVE image reports zero, which is how
/// pre-Pentium-4 parts spell "no mask field". Bit 6 (DAZ) is clear: a CPU old
/// enough to omit the field is old enough not to implement denormals-are-zero.
pub const MXCSR_MASK_DEFAULT: u32 = 0xFFBF;

/// Active XSAVE area size in bytes, for the features enabled in XCR0; 0
/// before `init()`.
#[inline]
pub fn area_size() -> usize {
    XSAVE_AREA_SIZE.load(Ordering::Relaxed)
}

/// Whether XSAVE is the active FPU save/restore mechanism — always `true`,
/// since a CPU without XSAVE panics in `init()`.
#[inline]
pub fn is_enabled() -> bool {
    true
}

/// The XCR0 value written to every CPU, or 0 before `init()`.
#[inline]
pub fn active_xcr0() -> u64 {
    ACTIVE_XCR0.load(Ordering::Relaxed)
}

/// Whether the compact `XSAVEC` instruction is available.
#[inline]
pub fn has_xsavec() -> bool {
    XSAVEC_AVAILABLE.load(Ordering::Relaxed)
}

/// Whether the optimised `XSAVEOPT` instruction is available.
#[inline]
pub fn has_xsaveopt() -> bool {
    XSAVEOPT_AVAILABLE.load(Ordering::Relaxed)
}

/// The MXCSR bits the BSP implements, taken to hold for every CPU. A word
/// carrying anything outside this mask faults on restore.
#[inline]
pub fn mxcsr_feature_mask() -> u32 {
    MXCSR_FEATURE_MASK.load(Ordering::Relaxed)
}

const FXSAVE_MXCSR_MASK_OFFSET: usize = 28;

/// Read this CPU's `MXCSR_MASK` out of a scratch FXSAVE image: CPUID reports
/// it nowhere, and `fxsave64` only writes memory, so the soft-float guarantee
/// (no writes to the XCR0-managed registers) still holds.
///
/// `#[inline(never)]` keeps the symbol name stable across build variants — the
/// vector gate keys its allowlist on it.
#[inline(never)]
fn detect_mxcsr_feature_mask() -> u32 {
    #[repr(C, align(16))]
    struct FxsaveArea([u8; FXSAVE_AREA_SIZE]);

    let mut area = FxsaveArea([0u8; FXSAVE_AREA_SIZE]);
    // SAFETY: `area` is a live, exclusively-borrowed, 16-byte-aligned buffer of
    // exactly the size `fxsave64` writes.
    unsafe {
        core::arch::asm!(
            "fxsave64 [{}]",
            in(reg) area.0.as_mut_ptr(),
            options(nostack),
        );
    }

    let mut raw = [0u8; 4];
    raw.copy_from_slice(&area.0[FXSAVE_MXCSR_MASK_OFFSET..FXSAVE_MXCSR_MASK_OFFSET + 4]);
    match u32::from_le_bytes(raw) {
        0 => MXCSR_MASK_DEFAULT,
        mask => mask,
    }
}

/// Enable XSAVE on the BSP and record the resulting configuration.
///
/// # Panics
/// Panics if the CPU does not support XSAVE.  Every x86-64 CPU since 2008
/// supports it, and QEMU always exposes it.  There is no FXSAVE fallback.
///
/// # Contract
/// * Must be called **once**, on the BSP, **before** SMP AP startup.
/// * Interrupts should be disabled (boot context).
pub fn init() -> i32 {
    let features = XsaveFeatures::detect();

    if !features.supported {
        panic!(
            "XSAVE: not supported by CPU — SlopOS requires XSAVE (available since 2008). \
             Cannot boot on this hardware."
        );
    }

    let supported = features.xcr0_supported;
    let mut xcr0 = Xcr0Flags::X87 | Xcr0Flags::SSE;

    if (supported & Xcr0Flags::AVX.bits()) != 0 {
        xcr0 |= Xcr0Flags::AVX;
    }

    let avx512_bits =
        Xcr0Flags::OPMASK.bits() | Xcr0Flags::ZMM_HI256.bits() | Xcr0Flags::HI16_ZMM.bits();
    if (supported & avx512_bits) == avx512_bits {
        xcr0 |= Xcr0Flags::OPMASK | Xcr0Flags::ZMM_HI256 | Xcr0Flags::HI16_ZMM;
    }

    // Must be visible before SMP startup: every AP replicates this mask.
    ACTIVE_XCR0.store(xcr0.bits(), Ordering::Release);

    let mask = Xcr0Mask::new(xcr0).expect("XCR0 mask built from this CPU's own CPUID report");
    let osxsave = Osxsave::enable();
    xcr0_write(&osxsave, mask);

    // CPUID.0Dh.0:EBX reports only the currently-enabled components, so this
    // must follow the XCR0 write.
    let area_size = crate::arch::x86_64::cpuid::xsave_area_size();
    XSAVE_AREA_SIZE.store(area_size, Ordering::Release);

    XSAVEC_AVAILABLE.store(features.xsavec, Ordering::Release);
    XSAVEOPT_AVAILABLE.store(features.xsaveopt, Ordering::Release);

    MXCSR_FEATURE_MASK.store(detect_mxcsr_feature_mask(), Ordering::Release);

    let _ = (area_size, supported);
    0
}

/// Replicate the BSP's XSAVE configuration on the current CPU.
///
/// # Contract
/// * `init()` must have been called first (on the BSP).
/// * Interrupts should be disabled.
pub fn enable_on_current_cpu() {
    let xcr0 = ACTIVE_XCR0.load(Ordering::Acquire);
    if xcr0 == 0 {
        return;
    }

    // Re-validated against *this* CPU's CPUID: an asymmetric core reporting
    // fewer XCR0 components would #GP on the write, and a panic naming the
    // mask beats a fault in the AP bring-up path.
    let mask = Xcr0Mask::new(Xcr0Flags::from_bits_truncate(xcr0))
        .expect("AP does not support the XCR0 components the BSP enabled");
    let osxsave = Osxsave::enable();
    xcr0_write(&osxsave, mask);
}
