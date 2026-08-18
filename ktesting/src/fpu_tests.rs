use slopos_ostd::cpu::x86_64::interrupts::IrqDisabled;
use slopos_ostd::cpu::x86_64::xsave;
use slopos_ostd::task::{FpuState, XSTATE_RESERVED_OFFSET};
use slopos_ostd::test_support::cpu_state;
use slopos_ostd::{klog_info, KBox};

use crate::TestResult;
use crate::{fail, pass};

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

/// A ring-0 `#GP` from `XRSTOR64` is recovered, not fatal.
///
/// A non-zero byte in the XSTATE header's reserved tail faults in both the
/// standard and the compacted form, unlike `XCOMP_BV`'s format bit, so the
/// fault does not depend on which XSAVE features the CPU implements.
fn fpu_xrstor_gp_is_recovered() -> TestResult {
    let Ok(mut saved) = KBox::try_init(FpuState::init_zero()) else {
        return fail!("could not allocate the live-state snapshot");
    };
    let Ok(mut poisoned) = KBox::try_init(FpuState::init_default()) else {
        return fail!("could not allocate the poisoned image");
    };
    poisoned.data[XSTATE_RESERVED_OFFSET] = 1;

    let xcr0 = xsave::active_xcr0();
    let pattern: cpu_state::Xmm128 = [0x_C0FF_EE00_1234_5678, 0x_8765_4321_00EE_FFC0];

    // The register file is architecturally undefined between the rejected
    // restore and the snapshot going back, so no context switch may observe it.
    let (rejected, restored, readback) = IrqDisabled::with(|_irq| {
        saved.save_current(xcr0);
        let rejected = !poisoned.restore_to_cpu(xcr0);
        let restored = saved.restore_to_cpu(xcr0);
        let readback = cpu_state::xmm0_roundtrip(pattern);
        let restored_again = saved.restore_to_cpu(xcr0);
        (rejected, restored && restored_again, readback)
    });

    if !rejected {
        return fail!("xrstor64 accepted an image with a dirty reserved header byte");
    }
    if !restored {
        return fail!("the register file did not take a known-good image back");
    }
    if readback != pattern {
        return fail!("the register file is unusable after a recovered #GP");
    }
    pass!()
}

crate::stest!(name = fpu_xmm_roundtrip_a);
crate::stest!(name = fpu_xmm_roundtrip_b);
crate::stest!(name = fpu_xrstor_gp_is_recovered);
