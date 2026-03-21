//! CPU feature detection via CPUID instruction.
//!
//! This module provides the CPUID instruction wrapper and constants for
//! feature flags used to detect hardware capabilities like APIC, x2APIC,
//! XSAVE, and other CPU features.
//!
//! Only flags actually referenced by kernel code are defined here.
//! Add new constants as needed when implementing feature detection.

// =============================================================================
// CPUID Instruction Wrapper
// =============================================================================

/// Execute CPUID with the given leaf (subleaf defaults to 0).
/// Returns (eax, ebx, ecx, edx).
#[inline(always)]
#[allow(unused_unsafe)]
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { core::arch::x86_64::__cpuid(leaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}

/// Execute CPUID with a specific leaf **and subleaf** (ECX).
///
/// Required for leaves that enumerate multiple sub-features, such as
/// leaf `0x0D` (XSAVE state enumeration) and leaf `0x07` (structured
/// extended features).
/// Returns (eax, ebx, ecx, edx).
#[inline(always)]
#[allow(unused_unsafe)]
pub fn cpuid_count(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}

// =============================================================================
// CPUID Leaf Numbers
// =============================================================================

/// Basic CPU information and feature flags.
pub const CPUID_LEAF_FEATURES: u32 = 0x01;

/// Structured extended feature flags (subleaf 0).
pub const CPUID_LEAF_STRUCTURED_EXT: u32 = 0x07;

/// XSAVE state enumeration (subleaf 0 = main, subleaf 1 = extended features).
pub const CPUID_LEAF_XSAVE: u32 = 0x0D;

/// Extended function information.
pub const CPUID_LEAF_EXT_INFO: u32 = 0x8000_0001;

// =============================================================================
// CPUID Leaf 1 - EDX Feature Flags
// =============================================================================

/// Physical Address Extension.
pub const CPUID_FEAT_EDX_PAE: u32 = 1 << 6;

/// APIC present (on-chip Advanced Programmable Interrupt Controller).
pub const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;

/// Page Global Enable.
pub const CPUID_FEAT_EDX_PGE: u32 = 1 << 13;

/// Page Attribute Table.
pub const CPUID_FEAT_EDX_PAT: u32 = 1 << 16;

// =============================================================================
// CPUID Leaf 1 - ECX Feature Flags
// =============================================================================

/// Process Context Identifiers (PCID).
pub const CPUID_FEAT_ECX_PCID: u32 = 1 << 17;

/// x2APIC support.
pub const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

/// XSAVE/XRSTOR/XGETBV/XSETBV instruction support.
pub const CPUID_FEAT_ECX_XSAVE: u32 = 1 << 26;

/// OS has enabled XSAVE via CR4.OSXSAVE.
/// When set, userland can execute XGETBV and the kernel has set CR4.OSXSAVE.
pub const CPUID_FEAT_ECX_OSXSAVE: u32 = 1 << 27;
// =============================================================================
// CPUID Leaf 7 (Subleaf 0) - EBX Structured Extended Feature Flags
// =============================================================================

/// INVPCID instruction support.
pub const CPUID_SEXT_EBX_INVPCID: u32 = 1 << 10;

// =============================================================================
// CPUID Extended Leaf 0x80000001 - EDX Flags
// =============================================================================

/// Long mode (64-bit).
pub const CPUID_EXT_FEAT_EDX_LM: u32 = 1 << 29;

// =============================================================================
// CPUID Leaf 0x0D, Subleaf 1 — XSAVE Extended Features (EAX)
// =============================================================================

/// XSAVEOPT: optimised XSAVE that only writes modified components.
pub const CPUID_XSAVE_EAX_XSAVEOPT: u32 = 1 << 0;

/// XSAVEC: compact XSAVE format (no gaps between components).
pub const CPUID_XSAVE_EAX_XSAVEC: u32 = 1 << 1;

/// XGETBV with ECX=1 supported (returns XCR0 AND IA32_XSS).
pub const CPUID_XSAVE_EAX_XGETBV_ECX1: u32 = 1 << 2;

/// XSAVES/XRSTORS and IA32_XSS MSR support (supervisor state components).
pub const CPUID_XSAVE_EAX_XSAVES: u32 = 1 << 3;

// =============================================================================
// XSAVE Feature Detection
// =============================================================================

/// Consolidated result of XSAVE feature detection.
///
/// Built by [`XsaveFeatures::detect`], which queries CPUID leaves `0x01`
/// and `0x0D` to determine XSAVE capability, supported XCR0 components,
/// and save-area sizes.
///
/// # Example (during boot)
/// ```ignore
/// let xf = XsaveFeatures::detect();
/// if xf.supported {
///     // Safe to set CR4.OSXSAVE and then write XCR0.
///     log!("XSAVE: max area {} bytes, features 0x{:x}",
///          xf.area_size_max, xf.xcr0_supported);
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct XsaveFeatures {
    /// CPU advertises XSAVE/XRSTOR via `CPUID.1:ECX[26]`.
    pub supported: bool,
    /// `XSAVEC` instruction available (compact format, no inter-component gaps).
    pub xsavec: bool,
    /// `XSAVEOPT` instruction available (only writes modified components).
    pub xsaveopt: bool,
    /// `XSAVES`/`XRSTORS` and `IA32_XSS` MSR supported.
    pub xsaves: bool,
    /// Bitmap of XCR0 feature bits the CPU supports (CPUID.0Dh.0:EAX|EDX).
    /// Use [`Xcr0Flags`](super::control_regs::Xcr0Flags) to interpret.
    pub xcr0_supported: u64,
    /// XSAVE area size for features *currently enabled* in XCR0
    /// (CPUID.0Dh.0:EBX).  Before `CR4.OSXSAVE` is set this reflects the
    /// reset default (x87+SSE only, typically 576 bytes).
    pub area_size_current: usize,
    /// Maximum XSAVE area size if **all** supported features are enabled
    /// (CPUID.0Dh.0:ECX).  Constant for a given CPU model.
    pub area_size_max: usize,
}

impl XsaveFeatures {
    /// Query CPUID and return the full XSAVE capability snapshot.
    ///
    /// Safe to call at any point during boot — reads CPUID only, does not
    /// write any control registers.
    pub fn detect() -> Self {
        // Step 1: Check basic XSAVE support (CPUID.1:ECX bit 26).
        let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
        let supported = (ecx1 & CPUID_FEAT_ECX_XSAVE) != 0;

        if !supported {
            return Self {
                supported: false,
                xsavec: false,
                xsaveopt: false,
                xsaves: false,
                xcr0_supported: 0,
                area_size_current: 0,
                area_size_max: 0,
            };
        }

        // Step 2: Query CPUID.0Dh subleaf 0 — supported XCR0 bits and area sizes.
        let (eax_0d, ebx_0d, ecx_0d, edx_0d) = cpuid_count(CPUID_LEAF_XSAVE, 0);
        let xcr0_supported = (eax_0d as u64) | ((edx_0d as u64) << 32);
        let area_size_current = ebx_0d as usize;
        let area_size_max = ecx_0d as usize;

        // Step 3: Query CPUID.0Dh subleaf 1 — extended XSAVE features.
        let (eax_0d1, _, _, _) = cpuid_count(CPUID_LEAF_XSAVE, 1);
        let xsaveopt = (eax_0d1 & CPUID_XSAVE_EAX_XSAVEOPT) != 0;
        let xsavec = (eax_0d1 & CPUID_XSAVE_EAX_XSAVEC) != 0;
        let xsaves = (eax_0d1 & CPUID_XSAVE_EAX_XSAVES) != 0;

        Self {
            supported,
            xsavec,
            xsaveopt,
            xsaves,
            xcr0_supported,
            area_size_current,
            area_size_max,
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience free functions (match plan’s exported API)
// ---------------------------------------------------------------------------

/// Return the XSAVE area size for the features **currently enabled** in XCR0.
///
/// This is a live query (`CPUID.0Dh.0:EBX`) and will change after `XCR0` is
/// modified to enable additional features (e.g. AVX).  Returns `0` when the
/// CPU does not support XSAVE.
#[inline]
pub fn xsave_area_size() -> usize {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    ebx as usize
}

/// Return the **maximum** XSAVE area size across all features the CPU
/// supports (`CPUID.0Dh.0:ECX`).  Constant for a given CPU model.
/// Returns `0` when the CPU does not support XSAVE.
#[inline]
pub fn xsave_max_size() -> usize {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (_, _, ecx, _) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    ecx as usize
}

/// Return the bitmap of XCR0 feature bits the CPU supports.
/// (`CPUID.0Dh.0:EAX` | `CPUID.0Dh.0:EDX << 32`).  Returns `0` when the
/// CPU does not support XSAVE.
#[inline]
pub fn xcr0_supported() -> u64 {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (eax, _, _, edx) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    (eax as u64) | ((edx as u64) << 32)
}
