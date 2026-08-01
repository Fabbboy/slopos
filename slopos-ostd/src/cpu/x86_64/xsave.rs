//! XSAVE / XRSTOR initialisation and runtime queries.
//!
//! Enables the XSAVE instruction family on the BSP during boot and on every
//! AP during SMP bring-up.
//!
//! **XSAVE is a hard boot requirement.**  If the CPU does not support XSAVE
//! the kernel panics during `init()`.  There is no FXSAVE fallback — every
//! x86-64 CPU since Intel Nehalem (2008) and AMD Bulldozer (2011) supports
//! XSAVE, and QEMU always exposes it.
//!
//! The module records three pieces of global state, all read through the
//! accessors below:
//!
//! * **`XSAVE_AREA_SIZE`** — runtime-detected save-area size (bytes).
//! * **`ACTIVE_XCR0`** — the XCR0 value written to every CPU, and the RFBM
//!   every `xsave64`/`xrstor64` in the kernel is issued with.
//! * **`MXCSR_FEATURE_MASK`** — the MXCSR bits this CPU implements, which is
//!   what an MXCSR word arriving from user memory is validated against.
//!
//! The `init()` entry point is called once on the BSP (via a boot step at
//! priority 42, before SMP).  Each AP then calls `enable_on_current_cpu()` to
//! replicate the same CR4 + XCR0 configuration.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::control_regs::{Osxsave, Xcr0Flags, Xcr0Mask, xcr0_write};
use crate::arch::x86_64::cpuid::XsaveFeatures;
use crate::task::FXSAVE_AREA_SIZE;

// ---------------------------------------------------------------------------
// Global state (set by BSP `init`, read by APs + task creation)
// ---------------------------------------------------------------------------

/// Active XSAVE area size in bytes.  Defaults to 0 until `init()` runs;
/// after init it reflects the hardware-reported size for the features
/// enabled in XCR0.
static XSAVE_AREA_SIZE: AtomicUsize = AtomicUsize::new(0);

/// XCR0 value computed by the BSP — every AP writes the same mask.
static ACTIVE_XCR0: AtomicU64 = AtomicU64::new(0);

/// `true` when `XSAVEC` is available (compact save format, no gaps).
static XSAVEC_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// `true` when `XSAVEOPT` is available (optimised — only writes dirty state).
static XSAVEOPT_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// The set of MXCSR bits this CPU implements. Every bit outside it is
/// reserved, and loading one raises `#GP` — which is what makes this the
/// authority for validating an MXCSR word that came from user memory.
///
/// Seeded with [`MXCSR_MASK_DEFAULT`] rather than zero so a read before
/// `init()` rejects reserved bits instead of rejecting everything.
static MXCSR_FEATURE_MASK: AtomicU32 = AtomicU32::new(MXCSR_MASK_DEFAULT);

/// The MXCSR mask to assume when the FXSAVE image reports zero, which is how
/// pre-Pentium-4 parts spell "no mask field". Bit 6 (DAZ) is clear: a CPU old
/// enough to omit the field is old enough not to implement denormals-are-zero.
pub const MXCSR_MASK_DEFAULT: u32 = 0xFFBF;

// ---------------------------------------------------------------------------
// Public queries
// ---------------------------------------------------------------------------

/// Active XSAVE area size in bytes.
///
/// Before `init()` this returns 0.  After `init()` it reflects the
/// hardware-reported size for the features enabled in XCR0.
#[inline]
pub fn area_size() -> usize {
    XSAVE_AREA_SIZE.load(Ordering::Relaxed)
}

/// Whether XSAVE is the active FPU save/restore mechanism.
///
/// Always returns `true` after a successful `init()`.  XSAVE is a hard
/// boot requirement — if the CPU does not support it, the kernel panics
/// before this function is ever reachable.
#[inline]
pub fn is_enabled() -> bool {
    // XSAVE is mandatory; if we booted, it is enabled.
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

/// The MXCSR bits the BSP implements — read once during `init()` and, like
/// XCR0, taken to hold for every CPU. An MXCSR word carrying anything outside
/// this mask faults on restore.
#[inline]
pub fn mxcsr_feature_mask() -> u32 {
    MXCSR_FEATURE_MASK.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// MXCSR feature-mask detection
// ---------------------------------------------------------------------------

/// Byte offset of `MXCSR_MASK` within the FXSAVE image.
const FXSAVE_MXCSR_MASK_OFFSET: usize = 28;

/// Read this CPU's `MXCSR_MASK` out of a scratch FXSAVE image.
///
/// The value is reported nowhere in CPUID; FXSAVE is the only instruction that
/// publishes it. `fxsave64` reads the register file and writes memory, so it
/// disturbs no live state — the soft-float guarantee is about writes to the
/// XCR0-managed registers, and this performs none.
///
/// `#[inline(never)]` so the enclosing symbol stays the same across build
/// variants: the vector gate keys its allowlist on that name, and an inlined
/// copy would attribute the instruction to whichever caller absorbed it.
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

// ---------------------------------------------------------------------------
// BSP initialisation (called once from boot step)
// ---------------------------------------------------------------------------

/// Detect XSAVE support, compute the XCR0 mask, enable CR4.OSXSAVE, write
/// XCR0, and record the resulting save-area size.
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

    // ------------------------------------------------------------------
    // 1. Compute the XCR0 mask: x87 + SSE are mandatory, AVX if available,
    //    AVX-512 if all three component bits are supported.
    // ------------------------------------------------------------------
    let supported = features.xcr0_supported;
    let mut xcr0 = Xcr0Flags::X87 | Xcr0Flags::SSE;

    if (supported & Xcr0Flags::AVX.bits()) != 0 {
        xcr0 |= Xcr0Flags::AVX;
    }

    // AVX-512 requires all three sub-components to be present.
    let avx512_bits =
        Xcr0Flags::OPMASK.bits() | Xcr0Flags::ZMM_HI256.bits() | Xcr0Flags::HI16_ZMM.bits();
    if (supported & avx512_bits) == avx512_bits {
        xcr0 |= Xcr0Flags::OPMASK | Xcr0Flags::ZMM_HI256 | Xcr0Flags::HI16_ZMM;
    }

    // Store for AP replication — must be visible before SMP startup.
    ACTIVE_XCR0.store(xcr0.bits(), Ordering::Release);

    // ------------------------------------------------------------------
    // 2. Enable XSAVE on the BSP: CR4.OSXSAVE then XCR0 write.
    // ------------------------------------------------------------------
    let mask = Xcr0Mask::new(xcr0).expect("XCR0 mask built from this CPU's own CPUID report");
    let osxsave = Osxsave::enable();
    xcr0_write(&osxsave, mask);

    // ------------------------------------------------------------------
    // 3. Re-query CPUID for the actual save-area size *after* XCR0 is set.
    //    CPUID.0Dh.0:EBX reflects the currently-enabled components.
    // ------------------------------------------------------------------
    let area_size = crate::arch::x86_64::cpuid::xsave_area_size();
    XSAVE_AREA_SIZE.store(area_size, Ordering::Release);

    // ------------------------------------------------------------------
    // 4. Record instruction variants for later context-switch selection.
    // ------------------------------------------------------------------
    XSAVEC_AVAILABLE.store(features.xsavec, Ordering::Release);
    XSAVEOPT_AVAILABLE.store(features.xsaveopt, Ordering::Release);

    // ------------------------------------------------------------------
    // 5. Record which MXCSR bits this CPU implements, for validating an
    //    MXCSR word that arrives from user memory.
    // ------------------------------------------------------------------
    MXCSR_FEATURE_MASK.store(detect_mxcsr_feature_mask(), Ordering::Release);

    let _ = (area_size, supported);
    0
}

// ---------------------------------------------------------------------------
// Per-CPU enablement (called on each AP from ap_entry)
// ---------------------------------------------------------------------------

/// Replicate the BSP's XSAVE configuration on the current CPU.
///
/// Sets CR4.OSXSAVE and writes the same XCR0 mask that `init()` computed.
///
/// # Contract
/// * `init()` must have been called first (on the BSP).
/// * Interrupts should be disabled.
pub fn enable_on_current_cpu() {
    let xcr0 = ACTIVE_XCR0.load(Ordering::Acquire);
    if xcr0 == 0 {
        return;
    }

    // Re-validate the BSP's mask against *this* CPU's CPUID rather than
    // trusting it: an asymmetric core that reports fewer XCR0 components
    // would take a #GP on the write, and a panic naming the mask beats a
    // fault in the AP bring-up path.
    let mask = Xcr0Mask::new(Xcr0Flags::from_bits_truncate(xcr0))
        .expect("AP does not support the XCR0 components the BSP enabled");
    let osxsave = Osxsave::enable();
    xcr0_write(&osxsave, mask);
}
