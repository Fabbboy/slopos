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
//! The module exposes two pieces of global state consumed by the context
//! switch assembly (`core/context_switch.s`):
//!
//! * **`XSAVE_AREA_SIZE`** — runtime-detected save-area size (bytes).
//! * **`ACTIVE_XCR0`** — the XCR0 value written to every CPU.
//!   Read by `context_switch.s` via `#[no_mangle]` for xsave64/xrstor64.
//!
//! The `init()` entry point is called once on the BSP (via a boot step at
//! priority 42, before SMP).  Each AP then calls `enable_on_current_cpu()` to
//! replicate the same CR4 + XCR0 configuration.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::control_regs::{read_cr4, write_cr4, xcr0_write, Cr4Flags, Xcr0Flags};
use super::cpuid::XsaveFeatures;
use crate::{klog_debug, klog_info};

// ---------------------------------------------------------------------------
// Global state (set by BSP `init`, read by APs + task creation)
// ---------------------------------------------------------------------------

/// Active XSAVE area size in bytes.  Defaults to 0 until `init()` runs;
/// after init it reflects the hardware-reported size for the features
/// enabled in XCR0.
static XSAVE_AREA_SIZE: AtomicUsize = AtomicUsize::new(0);

/// XCR0 value computed by the BSP — every AP writes the same mask.
///
/// # Assembly access
/// `context_switch.s` loads this into `EDX:EAX` before `xsave64`/`xrstor64`
/// to specify which state components to save/restore.
#[unsafe(no_mangle)]
pub static ACTIVE_XCR0: AtomicU64 = AtomicU64::new(0);

/// `true` when `XSAVEC` is available (compact save format, no gaps).
static XSAVEC_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// `true` when `XSAVEOPT` is available (optimised — only writes dirty state).
static XSAVEOPT_AVAILABLE: AtomicBool = AtomicBool::new(false);

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
    let cr4 = read_cr4() | Cr4Flags::OSXSAVE.bits();
    write_cr4(cr4);
    xcr0_write(xcr0.bits());

    // ------------------------------------------------------------------
    // 3. Re-query CPUID for the actual save-area size *after* XCR0 is set.
    //    CPUID.0Dh.0:EBX reflects the currently-enabled components.
    // ------------------------------------------------------------------
    let area_size = super::cpuid::xsave_area_size();
    XSAVE_AREA_SIZE.store(area_size, Ordering::Release);

    // ------------------------------------------------------------------
    // 4. Record instruction variants for later context-switch selection.
    // ------------------------------------------------------------------
    XSAVEC_AVAILABLE.store(features.xsavec, Ordering::Release);
    XSAVEOPT_AVAILABLE.store(features.xsaveopt, Ordering::Release);

    // ------------------------------------------------------------------
    // 5. Log.
    // ------------------------------------------------------------------
    klog_info!(
        "XSAVE: enabled, area size {} bytes, features 0x{:x}",
        area_size,
        xcr0.bits(),
    );
    klog_debug!(
        "XSAVE: supported XCR0 0x{:x}, max area {} B, XSAVEC={}, XSAVEOPT={}, XSAVES={}",
        supported,
        features.area_size_max,
        features.xsavec,
        features.xsaveopt,
        features.xsaves,
    );

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

    // Set CR4.OSXSAVE on this AP.
    let cr4 = read_cr4() | Cr4Flags::OSXSAVE.bits();
    write_cr4(cr4);

    // Write the identical XCR0 mask.
    xcr0_write(xcr0);
}
