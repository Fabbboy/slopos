use slopos_ostd::test_support::cpu_state;
use slopos_utils::klog_info;

use crate::TestResult;

fn fpu_xmm_roundtrip_a() -> TestResult {
    let pattern_lo: u64 = 0x_DEAD_BEEF_CAFE_BABE;
    let pattern_hi: u64 = 0x_1234_5678_9ABC_DEF0;
    let readback = cpu_state::xmm0_roundtrip([pattern_lo, pattern_hi]);

    if readback == [pattern_lo, pattern_hi] {
        TestResult::Pass
    } else {
        klog_info!("FPU: xmm roundtrip A mismatch");
        TestResult::Fail
    }
}

fn fpu_xmm_roundtrip_b() -> TestResult {
    let pattern2_lo: u64 = 0x_FFFF_0000_AAAA_5555;
    let pattern2_hi: u64 = 0x_0123_4567_89AB_CDEF;
    let readback = cpu_state::xmm1_roundtrip([pattern2_lo, pattern2_hi]);

    if readback == [pattern2_lo, pattern2_hi] {
        TestResult::Pass
    } else {
        klog_info!("FPU: xmm roundtrip B mismatch");
        TestResult::Fail
    }
}

crate::stest!(name = fpu_xmm_roundtrip_a);
crate::stest!(name = fpu_xmm_roundtrip_b);
