use slopos_utils::klog_info;

use crate::TestResult;

fn fpu_xmm_roundtrip_a() -> TestResult {
    use core::arch::x86_64::{__m128i, _mm_set_epi64x, _mm_storeu_si128};

    let pattern_lo: i64 = 0x_DEAD_BEEF_CAFE_BABE_u64 as i64;
    let pattern_hi: i64 = 0x_1234_5678_9ABC_DEF0_u64 as i64;
    let expected = unsafe { _mm_set_epi64x(pattern_hi, pattern_lo) };

    let readback: __m128i;
    unsafe {
        core::arch::asm!(
            "movdqa {tmp}, {src}",
            "movdqa xmm0, {tmp}",
            tmp = out(xmm_reg) _,
            src = in(xmm_reg) expected,
        );
        core::arch::asm!(
            "movdqa {dst}, xmm0",
            dst = out(xmm_reg) readback,
        );
    }

    let mut result = [0u8; 16];
    let mut expected_bytes = [0u8; 16];
    unsafe {
        _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback);
        _mm_storeu_si128(expected_bytes.as_mut_ptr() as *mut __m128i, expected);
    }
    if result == expected_bytes {
        TestResult::Pass
    } else {
        klog_info!("FPU: xmm roundtrip A mismatch");
        TestResult::Fail
    }
}

fn fpu_xmm_roundtrip_b() -> TestResult {
    use core::arch::x86_64::{__m128i, _mm_set_epi64x, _mm_storeu_si128};

    let pattern2_lo: i64 = 0x_FFFF_0000_AAAA_5555_u64 as i64;
    let pattern2_hi: i64 = 0x_0123_4567_89AB_CDEF_u64 as i64;
    let pattern2 = unsafe { _mm_set_epi64x(pattern2_hi, pattern2_lo) };

    let readback2: __m128i;
    unsafe {
        core::arch::asm!(
            "movdqa xmm1, {src}",
            "movdqa {dst}, xmm1",
            src = in(xmm_reg) pattern2,
            dst = out(xmm_reg) readback2,
        );
    }

    let mut result = [0u8; 16];
    let mut expected2_bytes = [0u8; 16];
    unsafe {
        _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback2);
        _mm_storeu_si128(expected2_bytes.as_mut_ptr() as *mut __m128i, pattern2);
    }
    if result == expected2_bytes {
        TestResult::Pass
    } else {
        klog_info!("FPU: xmm roundtrip B mismatch");
        TestResult::Fail
    }
}

crate::stest!(name = fpu_xmm_roundtrip_a);
crate::stest!(name = fpu_xmm_roundtrip_b);
