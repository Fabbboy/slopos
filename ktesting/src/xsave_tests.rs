//! XSAVE / FPU / SIMD regression tests.
//!
//! XSAVE is a hard boot requirement, so nothing here skips on its absence.

use crate::TestResult;
use crate::{fail, pass};
use slopos_arch::cpu::control_regs::{Cr4Flags, Osxsave, Xcr0Flags, read_cr4, xcr0_read};
use slopos_arch::cpu::cpuid::XsaveFeatures;
use slopos_arch::cpu::xsave;
use slopos_ostd::KBox;

/// 64-byte aligned XSAVE area large enough to cover up to AVX-512.
#[repr(C, align(64))]
#[derive(slopos_ostd::Zeroable)]
struct XsaveArea {
    data: [u8; 2688],
}

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

pub fn test_xsave_area_size_sane() -> TestResult {
    let size = xsave::area_size();

    if size < 512 {
        return fail!("area_size {} < 512 (FXSAVE minimum)", size);
    }

    let features = XsaveFeatures::detect();
    if size < features.area_size_current {
        return fail!(
            "area_size {} < CPUID current size {}",
            size,
            features.area_size_current
        );
    }
    if size > features.area_size_max {
        return fail!(
            "area_size {} > CPUID max size {}",
            size,
            features.area_size_max
        );
    }

    pass!()
}

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

pub fn test_xsave_features_consistency() -> TestResult {
    let f = XsaveFeatures::detect();

    if (f.xcr0_supported & Xcr0Flags::X87.bits()) == 0 {
        return fail!("xcr0_supported 0x{:x} missing X87 bit", f.xcr0_supported);
    }

    if f.area_size_max < f.area_size_current {
        return fail!(
            "area_size_max {} < area_size_current {}",
            f.area_size_max,
            f.area_size_current
        );
    }

    if f.area_size_current < 512 {
        return fail!(
            "area_size_current {} < 512 (x87+SSE minimum)",
            f.area_size_current
        );
    }

    pass!()
}

pub fn test_cr4_osxsave_set() -> TestResult {
    let cr4 = read_cr4();
    if (cr4 & Cr4Flags::OSXSAVE.bits()) == 0 {
        return fail!("XSAVE enabled but CR4.OSXSAVE not set (CR4=0x{:x})", cr4);
    }
    pass!()
}

pub fn test_xcr0_matches_active() -> TestResult {
    let expected = xsave::active_xcr0();
    // Reading XCR0 without CR4.OSXSAVE is a #UD.
    let Some(osxsave) = Osxsave::probe() else {
        return fail!("CR4.OSXSAVE is clear; XCR0 is not readable");
    };
    let actual = xcr0_read(&osxsave);

    if actual != expected {
        return fail!(
            "XCR0 mismatch: register=0x{:x}, active_xcr0()=0x{:x}",
            actual,
            expected
        );
    }
    pass!()
}

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

pub fn test_sse_xsave_xrstor_roundtrip() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0 = xsave::active_xcr0();
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

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

/// The upper YMM halves are what FXSAVE cannot save: a context switch that
/// regressed to FXSAVE would lose them silently.
pub fn test_avx_xsave_xrstor_roundtrip() -> TestResult {
    let xcr0 = xsave::active_xcr0();
    if (xcr0 & Xcr0Flags::AVX.bits()) == 0 {
        return TestResult::Skipped;
    }

    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

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

pub fn test_sse_multi_register_isolation() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");

    let xcr0 = xsave::active_xcr0();
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;

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

pub fn test_xsave_area_size_matches_cpuid() -> TestResult {
    let cpuid_size = slopos_arch::cpu::cpuid::xsave_area_size();
    let runtime_size = xsave::area_size();

    if cpuid_size == 0 {
        return fail!("CPUID xsave_area_size() returned 0 with XSAVE enabled");
    }

    // Equality holds because init() stores CPUID.0Dh.0:EBX directly.
    if runtime_size != cpuid_size {
        return fail!(
            "area_size mismatch: runtime={}, CPUID={}",
            runtime_size,
            cpuid_size
        );
    }

    pass!()
}

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

/// The MXCSR mask read at boot must cover the kernel's own default, or
/// `validate_xsave_image` rejects the init image written into every new task.
pub fn test_mxcsr_feature_mask_covers_kernel_default() -> TestResult {
    let mask = xsave::mxcsr_feature_mask();

    if mask == 0 {
        return fail!("mxcsr_feature_mask() is zero — boot detection did not run");
    }
    if slopos_ostd::task::MXCSR_DEFAULT & !mask != 0 {
        return fail!(
            "mxcsr_feature_mask() {:#x} does not cover the kernel default {:#x}",
            mask,
            slopos_ostd::task::MXCSR_DEFAULT
        );
    }
    pass!()
}

/// The image `xsave64` itself produces must pass `validate_xsave_image`: the
/// validator gates every signal return.
pub fn test_validate_accepts_live_xsave_image() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");
    let xcr0 = xsave::active_xcr0();

    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        slopos_ostd::test_support::cpu_state::xsave_to(&mut area.data, xcr0);
    });

    match slopos_ostd::task::validate_xsave_image(&area.data, xcr0, xsave::mxcsr_feature_mask()) {
        Ok(()) => pass!(),
        Err(err) => fail!(
            "validate_xsave_image rejected a live xsave64 image: {:?}",
            err
        ),
    }
}

/// ...and must reject that same image once its XSTATE header is poisoned: a
/// validator that accepts everything would pass the test above too.
pub fn test_validate_rejects_poisoned_live_image() -> TestResult {
    let mut area: KBox<XsaveArea> = KBox::zeroed().expect("alloc");
    let xcr0 = xsave::active_xcr0();

    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        slopos_ostd::test_support::cpu_state::xsave_to(&mut area.data, xcr0);
    });

    area.data[slopos_ostd::task::XCOMP_BV_OFFSET + 7] = 0x80;

    match slopos_ostd::task::validate_xsave_image(&area.data, xcr0, xsave::mxcsr_feature_mask()) {
        Err(slopos_ostd::task::XsaveImageError::Compacted) => pass!(),
        other => fail!("a compacted-format XCOMP_BV was not rejected: {:?}", other),
    }
}

crate::stest!(name = test_xsave_enabled_matches_cpuid, suite = xsave);
crate::stest!(name = test_xsave_area_size_sane, suite = xsave);
crate::stest!(name = test_xsave_xcr0_mandatory_bits, suite = xsave);
crate::stest!(name = test_xsave_features_consistency, suite = xsave);
crate::stest!(name = test_cr4_osxsave_set, suite = xsave);
crate::stest!(name = test_xcr0_matches_active, suite = xsave);
crate::stest!(name = test_xcr0_avx_consistent, suite = xsave);
crate::stest!(name = test_sse_xsave_xrstor_roundtrip, suite = xsave);
crate::stest!(name = test_avx_xsave_xrstor_roundtrip, suite = xsave);
crate::stest!(name = test_sse_multi_register_isolation, suite = xsave);
crate::stest!(name = test_xsave_area_size_matches_cpuid, suite = xsave);
crate::stest!(name = test_xsave_area_size_covers_avx, suite = xsave);
crate::stest!(name = test_xsave_variant_flags_consistent, suite = xsave);
crate::stest!(
    name = test_mxcsr_feature_mask_covers_kernel_default,
    suite = xsave
);
crate::stest!(name = test_validate_accepts_live_xsave_image, suite = xsave);
crate::stest!(
    name = test_validate_rejects_poisoned_live_image,
    suite = xsave
);
