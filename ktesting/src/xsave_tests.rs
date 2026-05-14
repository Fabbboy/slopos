//! XSAVE / FPU / SIMD regression tests.
//!
//! These tests verify:
//! - XSAVE feature detection matches CPUID reality
//! - CR4.OSXSAVE and XCR0 are configured correctly on the running CPU
//! - SSE register state survives an XSAVE→XRSTOR round-trip
//! - AVX register state (upper YMM halves) survives an XSAVE→XRSTOR round-trip
//! - Multiple registers remain isolated through save/restore
//! - The XSAVE area size reported at runtime is sane
//!
//! XSAVE is a hard boot requirement — the kernel panics during init if the
//! CPU does not support it.  These tests therefore never need to skip due to
//! XSAVE being unavailable; they run unconditionally.
//!
//! All tests run on the BSP in the test harness context (single CPU, no
//! context switch).  The context-switch assembly macros use the same XSAVE
//! codepath tested here, so regressions in the instruction encoding or
//! component mask will be caught.

use crate::TestResult;
use crate::{fail, pass};
use slopos_arch::cpu::control_regs::{read_cr4, xcr0_read, Cr4Flags, Xcr0Flags};
use slopos_arch::cpu::cpuid::XsaveFeatures;
use slopos_arch::cpu::xsave;
use slopos_ostd::KBox;

/// 64-byte aligned XSAVE area large enough to cover up to AVX-512.
#[repr(C, align(64))]
#[derive(slopos_ostd::Zeroable)]
struct XsaveArea {
    data: [u8; 2688],
}

// =============================================================================
// 1. XSAVE Detection Sanity
// =============================================================================

/// Verify that `xsave::is_enabled()` is always true (XSAVE is mandatory)
/// and that CPUID agrees.
pub fn test_xsave_enabled_matches_cpuid() -> TestResult {
    let features = XsaveFeatures::detect();

    if !features.supported {
        return fail!("CPUID says XSAVE unsupported but we booted (XSAVE is mandatory)");
    }
    if !xsave::is_enabled() {
        return fail!("xsave::is_enabled() returned false but XSAVE is a boot requirement");
    }
    pass!()
}

/// Verify area_size() returns a sane value (>= 512 for FXSAVE compat,
/// and >= CPUID-reported current size).
pub fn test_xsave_area_size_sane() -> TestResult {
    let size = xsave::area_size();

    // Minimum: FXSAVE area is 512 bytes.
    if size < 512 {
        return fail!("area_size {} < 512 (FXSAVE minimum)", size);
    }

    let features = XsaveFeatures::detect();
    // With XSAVE enabled and XCR0 set, the runtime size should match
    // what CPUID reports for the currently-enabled features.
    if size < features.area_size_current {
        return fail!(
            "area_size {} < CPUID current size {}",
            size,
            features.area_size_current
        );
    }
    // Must not exceed the hardware maximum.
    if size > features.area_size_max {
        return fail!(
            "area_size {} > CPUID max size {}",
            size,
            features.area_size_max
        );
    }

    pass!()
}

/// Verify active_xcr0() has at least x87+SSE bits set (mandatory).
pub fn test_xsave_xcr0_mandatory_bits() -> TestResult {
    let xcr0 = xsave::active_xcr0();
    let x87_sse = Xcr0Flags::X87.bits() | Xcr0Flags::SSE.bits();

    if (xcr0 & x87_sse) != x87_sse {
        return fail!(
            "active_xcr0 0x{:x} missing mandatory x87|SSE bits (need 0x{:x})",
            xcr0,
            x87_sse
        );
    }
    pass!()
}

/// Verify that the XsaveFeatures struct is internally consistent.
pub fn test_xsave_features_consistency() -> TestResult {
    let f = XsaveFeatures::detect();

    // Supported XCR0 must include x87 (bit 0) — hardware enforces this.
    if (f.xcr0_supported & Xcr0Flags::X87.bits()) == 0 {
        return fail!("xcr0_supported 0x{:x} missing X87 bit", f.xcr0_supported);
    }

    // Max area size must be >= current area size.
    if f.area_size_max < f.area_size_current {
        return fail!(
            "area_size_max {} < area_size_current {}",
            f.area_size_max,
            f.area_size_current
        );
    }

    // Current area size must be at least 512 (x87+SSE header).
    if f.area_size_current < 512 {
        return fail!(
            "area_size_current {} < 512 (x87+SSE minimum)",
            f.area_size_current
        );
    }

    pass!()
}

// =============================================================================
// 2. CR4 / XCR0 Consistency on Current CPU
// =============================================================================

/// Verify CR4.OSXSAVE is set on the currently-running CPU.
pub fn test_cr4_osxsave_set() -> TestResult {
    let cr4 = read_cr4();
    if (cr4 & Cr4Flags::OSXSAVE.bits()) == 0 {
        return fail!("XSAVE enabled but CR4.OSXSAVE not set (CR4=0x{:x})", cr4);
    }
    pass!()
}

/// Verify that the live XCR0 register matches what xsave::active_xcr0() reports.
pub fn test_xcr0_matches_active() -> TestResult {
    let expected = xsave::active_xcr0();
    let actual = xcr0_read();

    if actual != expected {
        return fail!(
            "XCR0 mismatch: register=0x{:x}, active_xcr0()=0x{:x}",
            actual,
            expected
        );
    }
    pass!()
}

/// If AVX is reported in active_xcr0, verify the AVX bit is in the
/// CPU's supported set.
pub fn test_xcr0_avx_consistent() -> TestResult {
    let xcr0 = xsave::active_xcr0();
    let features = XsaveFeatures::detect();

    let avx_bit = Xcr0Flags::AVX.bits();
    if (xcr0 & avx_bit) != 0 && (features.xcr0_supported & avx_bit) == 0 {
        return fail!(
            "AVX enabled in XCR0 (0x{:x}) but not supported by CPU (0x{:x})",
            xcr0,
            features.xcr0_supported
        );
    }
    pass!()
}

// =============================================================================
// 3. SSE Register State — XSAVE/XRSTOR Round-Trip
// =============================================================================

/// Write known patterns to XMM0-XMM3, xsave to a buffer, zero the
/// registers, xrstor from the buffer, and verify the patterns survive.
pub fn test_sse_xsave_xrstor_roundtrip() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0 = xsave::active_xcr0();
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

    // 4 x 128-bit patterns stored as contiguous memory (loaded via movdqu).
    #[repr(C, align(16))]
    struct Patterns {
        data: [[u64; 2]; 4],
    }
    let patterns = Patterns {
        data: [
            [0xDEAD_BEEF_CAFE_BABE, 0x1234_5678_9ABC_DEF0],
            [0xAAAA_5555_BBBB_6666, 0xCCCC_7777_DDDD_8888],
            [0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210],
            [0xFFFF_0000_AAAA_5555, 0x0000_FFFF_5555_AAAA],
        ],
    };

    let readback = slopos_ostd::test_support::cpu_state::sse_xsave_xrstor_roundtrip_4(
        &patterns.data,
        &mut area.data,
        xcr0,
    );
    let _ = (xcr0_lo, xcr0_hi);

    for i in 0..4 {
        if patterns.data[i] != readback[i] {
            return fail!(
                "XMM{} mismatch after XSAVE/XRSTOR: expected ({:016x},{:016x}), got ({:016x},{:016x})",
                i,
                patterns.data[i][0],
                patterns.data[i][1],
                readback[i][0],
                readback[i][1]
            );
        }
    }

    pass!()
}

// =============================================================================
// 4. AVX Register State — XSAVE/XRSTOR Round-Trip (Upper YMM)
// =============================================================================

/// Write known patterns to the upper 128 bits of YMM0-YMM1 (the part that
/// FXSAVE cannot save), xsave, zero, xrstor, and verify.
///
/// This is THE critical regression test — if the context switch were still
/// using FXSAVE, the upper halves would be silently lost.
pub fn test_avx_xsave_xrstor_roundtrip() -> TestResult {
    // Check AVX is actually enabled in XCR0.
    let xcr0 = xsave::active_xcr0();
    if (xcr0 & Xcr0Flags::AVX.bits()) == 0 {
        return TestResult::Skipped;
    }

    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

    // Two 256-bit patterns laid out as contiguous 128-bit halves in memory.
    // ymm_patterns[0] = YMM0 lower 128, ymm_patterns[1] = YMM0 upper 128,
    // ymm_patterns[2] = YMM1 lower 128, ymm_patterns[3] = YMM1 upper 128.
    #[repr(C, align(16))]
    struct YmmPatterns {
        data: [[u64; 2]; 4],
    }
    let patterns = YmmPatterns {
        data: [
            [0xDEAD_BEEF_CAFE_BABE, 0x1111_2222_3333_4444], // YMM0 lower
            [0xAAAA_BBBB_CCCC_DDDD, 0x5555_6666_7777_8888], // YMM0 upper
            [0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210], // YMM1 lower
            [0xF0F0_E0E0_D0D0_C0C0, 0xA0A0_B0B0_9090_8080], // YMM1 upper
        ],
    };

    let readback = slopos_ostd::test_support::cpu_state::avx_xsave_xrstor_roundtrip_2ymm(
        &patterns.data,
        &mut area.data,
        xcr0,
    );
    let _ = (xcr0_lo, xcr0_hi);

    let labels = ["YMM0 lower", "YMM0 UPPER", "YMM1 lower", "YMM1 UPPER"];
    for i in 0..4 {
        if patterns.data[i] != readback[i] {
            return fail!(
                "{} mismatch: expected ({:016x},{:016x}), got ({:016x},{:016x})",
                labels[i],
                patterns.data[i][0],
                patterns.data[i][1],
                readback[i][0],
                readback[i][1]
            );
        }
    }

    pass!()
}

// =============================================================================
// 5. Multi-Register Isolation
// =============================================================================

/// Verify that XSAVE/XRSTOR preserves many SSE registers independently
/// (XMM0 through XMM7 with distinct patterns).
pub fn test_sse_multi_register_isolation() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0 = xsave::active_xcr0();
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

    // 8 distinct 128-bit patterns for XMM0-XMM7.
    // Stored as contiguous memory, loaded via movdqu.
    #[repr(C, align(16))]
    struct MultiPatterns {
        data: [[u64; 2]; 8],
    }
    let patterns = MultiPatterns {
        data: [
            [0x1111_1111_1111_1111, 0xEEEE_EEEE_EEEE_EEEE],
            [0x2222_2222_2222_2222, 0xDDDD_DDDD_DDDD_DDDD],
            [0x3333_3333_3333_3333, 0xCCCC_CCCC_CCCC_CCCC],
            [0x4444_4444_4444_4444, 0xBBBB_BBBB_BBBB_BBBB],
            [0x5555_5555_5555_5555, 0xAAAA_AAAA_AAAA_AAAA],
            [0x6666_6666_6666_6666, 0x9999_9999_9999_9999],
            [0x7777_7777_7777_7777, 0x8888_8888_8888_8888],
            [0xDEAD_BEEF_CAFE_BABE, 0x0123_4567_89AB_CDEF],
        ],
    };

    let readback = slopos_ostd::test_support::cpu_state::sse_xsave_xrstor_roundtrip_8(
        &patterns.data,
        &mut area.data,
        xcr0,
    );
    let _ = (xcr0_lo, xcr0_hi);

    for i in 0..8 {
        if patterns.data[i] != readback[i] {
            return fail!(
                "XMM{} mismatch: expected ({:016x},{:016x}), got ({:016x},{:016x})",
                i,
                patterns.data[i][0],
                patterns.data[i][1],
                readback[i][0],
                readback[i][1]
            );
        }
    }

    pass!()
}

// =============================================================================
// 6. XSAVE Area Size Matches Enabled Features
// =============================================================================

/// Cross-check the runtime area size against what CPUID reports for the
/// features currently enabled in XCR0.
pub fn test_xsave_area_size_matches_cpuid() -> TestResult {
    // CPUID.0Dh.0:EBX = size for currently-enabled XCR0 features.
    let cpuid_size = slopos_arch::cpu::cpuid::xsave_area_size();
    let runtime_size = xsave::area_size();

    if cpuid_size == 0 {
        return fail!("CPUID xsave_area_size() returned 0 with XSAVE enabled");
    }

    // They should be equal — init() stores CPUID.0Dh.0:EBX directly.
    if runtime_size != cpuid_size {
        return fail!(
            "area_size mismatch: runtime={}, CPUID={}",
            runtime_size,
            cpuid_size
        );
    }

    pass!()
}

/// Verify the area size is large enough for AVX if AVX is enabled.
/// AVX requires at least 832 bytes (512 legacy + 64 header + 256 YMM).
pub fn test_xsave_area_size_covers_avx() -> TestResult {
    let xcr0 = xsave::active_xcr0();
    if (xcr0 & Xcr0Flags::AVX.bits()) == 0 {
        return TestResult::Skipped;
    }

    let size = xsave::area_size();
    // x87 (160) + SSE header (352) + AVX YMM_Hi128 (256) + XSAVE header (64) = 832
    if size < 832 {
        return fail!("AVX enabled but area_size {} < 832 (minimum for AVX)", size);
    }

    pass!()
}

/// Verify XSAVEC and XSAVEOPT flags match between XsaveFeatures and xsave module.
pub fn test_xsave_variant_flags_consistent() -> TestResult {
    let features = XsaveFeatures::detect();

    if features.xsavec != xsave::has_xsavec() {
        return fail!(
            "XSAVEC mismatch: CPUID={}, module={}",
            features.xsavec,
            xsave::has_xsavec()
        );
    }

    if features.xsaveopt != xsave::has_xsaveopt() {
        return fail!(
            "XSAVEOPT mismatch: CPUID={}, module={}",
            features.xsaveopt,
            xsave::has_xsaveopt()
        );
    }

    pass!()
}

// =============================================================================
// Suite Registration
// =============================================================================

// Detection sanity
crate::stest!(name = test_xsave_enabled_matches_cpuid, suite = xsave);
crate::stest!(name = test_xsave_area_size_sane, suite = xsave);
crate::stest!(name = test_xsave_xcr0_mandatory_bits, suite = xsave);
crate::stest!(name = test_xsave_features_consistency, suite = xsave);
// CR4 / XCR0 consistency
crate::stest!(name = test_cr4_osxsave_set, suite = xsave);
crate::stest!(name = test_xcr0_matches_active, suite = xsave);
crate::stest!(name = test_xcr0_avx_consistent, suite = xsave);
// SSE round-trip
crate::stest!(name = test_sse_xsave_xrstor_roundtrip, suite = xsave);
// AVX round-trip (THE critical test)
crate::stest!(name = test_avx_xsave_xrstor_roundtrip, suite = xsave);
// Multi-register isolation
crate::stest!(name = test_sse_multi_register_isolation, suite = xsave);
// Area size verification
crate::stest!(name = test_xsave_area_size_matches_cpuid, suite = xsave);
crate::stest!(name = test_xsave_area_size_covers_avx, suite = xsave);
// Variant flags
crate::stest!(name = test_xsave_variant_flags_consistent, suite = xsave);
