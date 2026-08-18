//! CPU feature detection via the CPUID instruction.
//!
//! Only flags actually referenced by kernel code are defined here.

/// Execute CPUID with the given leaf; returns `(eax, ebx, ecx, edx)`.
#[inline(always)]
#[allow(unused_unsafe)]
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { core::arch::x86_64::__cpuid(leaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}

/// Execute CPUID with a specific leaf **and subleaf** (ECX); returns
/// `(eax, ebx, ecx, edx)`.
#[inline(always)]
#[allow(unused_unsafe)]
pub fn cpuid_count(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}

pub const CPUID_LEAF_FEATURES: u32 = 0x01;

pub const CPUID_LEAF_STRUCTURED_EXT: u32 = 0x07;

/// XSAVE state enumeration (subleaf 0 = main, subleaf 1 = extended features).
pub const CPUID_LEAF_XSAVE: u32 = 0x0D;

pub const CPUID_LEAF_EXT_INFO: u32 = 0x8000_0001;

pub const CPUID_FEAT_EDX_PAE: u32 = 1 << 6;

pub const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;

pub const CPUID_FEAT_EDX_PGE: u32 = 1 << 13;

pub const CPUID_FEAT_EDX_PAT: u32 = 1 << 16;

pub const CPUID_FEAT_ECX_PCID: u32 = 1 << 17;

pub const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

pub const CPUID_FEAT_ECX_XSAVE: u32 = 1 << 26;

pub const CPUID_FEAT_ECX_RDRAND: u32 = 1 << 30;

/// OS has enabled XSAVE via CR4.OSXSAVE; when set, userland can execute XGETBV.
pub const CPUID_FEAT_ECX_OSXSAVE: u32 = 1 << 27;

pub const CPUID_SEXT_EBX_SMEP: u32 = 1 << 7;

pub const CPUID_SEXT_EBX_INVPCID: u32 = 1 << 10;

pub const CPUID_SEXT_EBX_RDSEED: u32 = 1 << 18;

pub const CPUID_SEXT_EBX_SMAP: u32 = 1 << 20;

pub const CPUID_EXT_FEAT_EDX_LM: u32 = 1 << 29;

/// XSAVEOPT: optimised XSAVE that only writes modified components.
pub const CPUID_XSAVE_EAX_XSAVEOPT: u32 = 1 << 0;

/// XSAVEC: compact XSAVE format (no gaps between components).
pub const CPUID_XSAVE_EAX_XSAVEC: u32 = 1 << 1;

/// XGETBV with ECX=1 supported (returns XCR0 AND IA32_XSS).
pub const CPUID_XSAVE_EAX_XGETBV_ECX1: u32 = 1 << 2;

/// XSAVES/XRSTORS and IA32_XSS MSR support (supervisor state components).
pub const CPUID_XSAVE_EAX_XSAVES: u32 = 1 << 3;

/// Consolidated result of XSAVE feature detection.
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
    pub xsavec: bool,
    pub xsaveopt: bool,
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
    /// Safe to call at any point during boot — reads CPUID only, does not
    /// write any control registers.
    pub fn detect() -> Self {
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

        let (eax_0d, ebx_0d, ecx_0d, edx_0d) = cpuid_count(CPUID_LEAF_XSAVE, 0);
        let xcr0_supported = (eax_0d as u64) | ((edx_0d as u64) << 32);
        let area_size_current = ebx_0d as usize;
        let area_size_max = ecx_0d as usize;

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

/// XSAVE area size for the features **currently enabled** in XCR0: a live
/// query (`CPUID.0Dh.0:EBX`) whose result changes as XCR0 gains features.
/// `0` when the CPU does not support XSAVE.
#[inline]
pub fn xsave_area_size() -> usize {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    ebx as usize
}

/// **Maximum** XSAVE area size across all features the CPU supports
/// (`CPUID.0Dh.0:ECX`); constant for a given CPU model, `0` without XSAVE.
#[inline]
pub fn xsave_max_size() -> usize {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (_, _, ecx, _) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    ecx as usize
}

/// Bitmap of XCR0 feature bits the CPU supports
/// (`CPUID.0Dh.0:EAX` | `CPUID.0Dh.0:EDX << 32`); `0` without XSAVE.
#[inline]
pub fn xcr0_supported() -> u64 {
    let (_, _, ecx1, _) = cpuid(CPUID_LEAF_FEATURES);
    if (ecx1 & CPUID_FEAT_ECX_XSAVE) == 0 {
        return 0;
    }
    let (eax, _, _, edx) = cpuid_count(CPUID_LEAF_XSAVE, 0);
    (eax as u64) | ((edx as u64) << 32)
}

/// Read the CPU vendor string from CPUID leaf 0 (e.g., "GenuineIntel", "AuthenticAMD").
pub fn cpu_vendor_string() -> [u8; 16] {
    let mut vendor = [0u8; 16];
    let (_, ebx, ecx, edx) = cpuid(0);
    vendor[0..4].copy_from_slice(&ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&ecx.to_le_bytes());
    vendor
}

/// Read the CPU brand string from CPUID leaves 0x80000002-0x80000004; all
/// zeros if extended CPUID is not supported.
pub fn cpu_brand_string() -> [u8; 48] {
    let mut brand = [0u8; 48];
    let (max_ext, _, _, _) = cpuid(0x8000_0000);
    if max_ext < 0x8000_0004 {
        return brand;
    }
    for i in 0u32..3 {
        let (eax, ebx, ecx, edx) = cpuid(0x8000_0002 + i);
        let off = (i as usize) * 16;
        brand[off..off + 4].copy_from_slice(&eax.to_le_bytes());
        brand[off + 4..off + 8].copy_from_slice(&ebx.to_le_bytes());
        brand[off + 8..off + 12].copy_from_slice(&ecx.to_le_bytes());
        brand[off + 12..off + 16].copy_from_slice(&edx.to_le_bytes());
    }
    brand
}

/// Extract CPU family, model, stepping from CPUID leaf 1.
pub fn cpu_family_model_stepping() -> (u8, u8, u8) {
    let (eax, _, _, _) = cpuid(CPUID_LEAF_FEATURES);
    let stepping = (eax & 0xF) as u8;
    let mut model = ((eax >> 4) & 0xF) as u8;
    let mut family = ((eax >> 8) & 0xF) as u8;
    let ext_model = ((eax >> 16) & 0xF) as u8;
    let ext_family = ((eax >> 20) & 0xFF) as u8;
    if family == 0x06 || family == 0x0F {
        model += ext_model << 4;
    }
    if family == 0x0F {
        family += ext_family;
    }
    (family, model, stepping)
}

/// CPU feature flags from CPUID leaf 1: bits 0-31 ECX, bits 32-63 EDX.
pub fn cpu_features_bitmask() -> u64 {
    let (_, _, ecx, edx) = cpuid(CPUID_LEAF_FEATURES);
    (ecx as u64) | ((edx as u64) << 32)
}
