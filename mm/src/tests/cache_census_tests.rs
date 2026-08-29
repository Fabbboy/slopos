//! Every CPU must carry the same memory-type configuration.

use slopos_testing::{TestResult, assert_test};

use crate::cache_census::{FB_PAT_INDEX, cpu_state, expected_pat, memory_type_name, pat_entry};

pub fn test_pat_entry_decodes_sdm_layout() -> TestResult {
    let pat = 0x0007_0406_0007_0401u64;
    assert_test!(pat_entry(pat, 0) == 0x01, "index 0");
    assert_test!(pat_entry(pat, 1) == 0x04, "index 1");
    assert_test!(pat_entry(pat, 7) == 0x00, "index 7");
    assert_test!(memory_type_name(0x01) == "WC", "WC name");
    assert_test!(memory_type_name(0x04) == "WT", "WT name");
    TestResult::Pass
}

pub fn test_every_cpu_carries_the_expected_pat() -> TestResult {
    let expected = expected_pat();
    let count = slopos_ostd::cpu::x86_64::pcr::get_cpu_count();

    let mut checked = 0u32;
    for cpu in 0..count {
        let Some(state) = cpu_state(cpu) else {
            continue;
        };
        checked += 1;
        assert_test!(
            state.pat == expected,
            "cpu {} has PAT=0x{:016x}, expected 0x{:016x}: framebuffer stores are {} there and {} on the BSP",
            cpu,
            state.pat,
            expected,
            memory_type_name(pat_entry(state.pat, FB_PAT_INDEX)),
            memory_type_name(pat_entry(expected, FB_PAT_INDEX))
        );
    }

    assert_test!(
        checked > 0,
        "no CPU reported a PAT sample -- the census never ran"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_pat_entry_decodes_sdm_layout,
    suite = cache_census
);
slopos_testing::stest!(
    name = test_every_cpu_carries_the_expected_pat,
    suite = cache_census
);
