use core::ffi::c_char;

use slopos_testing::{TestSuiteDesc, TestSuiteResult, measure_elapsed_ms};

const FPU_NAME: &[u8] = b"fpu_sse\0";

fn run_fpu_suite(_config: *const (), out: *mut TestSuiteResult) -> i32 {
    use core::arch::x86_64::{__m128i, _mm_set_epi64x, _mm_storeu_si128};

    let start = slopos_arch::tsc::rdtsc();
    let total = 2u32;
    let mut passed = 0u32;

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
        passed += 1;
    }

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

    let mut expected2_bytes = [0u8; 16];
    unsafe {
        _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback2);
        _mm_storeu_si128(expected2_bytes.as_mut_ptr() as *mut __m128i, pattern2);
    }
    if result == expected2_bytes {
        passed += 1;
    }

    let elapsed = measure_elapsed_ms(start, slopos_arch::tsc::rdtsc());
    if let Some(out_ref) = unsafe { out.as_mut() } {
        out_ref.name = FPU_NAME.as_ptr() as *const c_char;
        out_ref.total = total;
        out_ref.passed = passed;
        out_ref.failed = total.saturating_sub(passed);
        out_ref.elapsed_ms = elapsed;
    }
    if passed == total { 0 } else { -1 }
}

#[used]
#[unsafe(link_section = ".test_registry")]
static FPU_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
    name: FPU_NAME.as_ptr() as *const c_char,
    run: Some(run_fpu_suite),
};
