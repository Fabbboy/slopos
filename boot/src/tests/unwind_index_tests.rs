//! The unwinder resolves return addresses through a binary-search index. Two
//! checks: the index's shape, and the per-frame cost of a real caught panic.

use slopos_kernel_services::clock;
use slopos_ostd::panic_recovery;
use slopos_ostd::test_support::unwind_index as index;
use slopos_ostd::test_support::unwind_index::{ENC_DATAREL_SDATA4, ENC_PCREL_SDATA4, ENC_UDATA4};
use slopos_testing::{TestResult, assert_test};

/// Frames added between the two timed catches. Each costs two FDE lookups —
/// `_Unwind_RaiseException` resolves every frame in both of its phases.
const DEPTH_DELTA: u32 = 20;
const SHALLOW_DEPTH: u32 = 2;

/// Budget for the extra frames' lookups: a linear scan costs tens of ms each at
/// opt-level 0, an indexed lookup microseconds — one bound fits any accelerator.
const DELTA_BUDGET_NS: u64 = 200_000_000;

#[inline(never)]
fn recurse_then_panic(depth: u32) {
    if depth == 0 {
        panic!("unwind index probe");
    }
    recurse_then_panic(core::hint::black_box(depth) - 1);
    // Defeats the tail call, so `depth` frames really are on the stack.
    core::hint::black_box(depth);
}

/// Elapsed nanoseconds for one caught panic thrown `depth` frames down.
fn time_catch(depth: u32) -> u64 {
    let start = clock::monotonic_ns();
    let _ = panic_recovery::run_recoverable(|| recurse_then_panic(depth));
    clock::monotonic_ns().saturating_sub(start)
}

/// `.eh_frame_hdr` holds a `datarel|sdata4` search table, and the finder uses it.
pub fn test_unwind_index_is_present() -> TestResult {
    let hdr = index::header();

    assert_test!(hdr.version == 1, "unwind index version is not 1");
    assert_test!(
        hdr.eh_frame_ptr_enc == ENC_PCREL_SDATA4,
        "eh_frame pointer is not pcrel|sdata4"
    );
    assert_test!(hdr.fde_count_enc == ENC_UDATA4, "fde_count is not udata4");
    assert_test!(
        hdr.table_enc == ENC_DATAREL_SDATA4,
        "search table is not datarel|sdata4 — a header without a table falls \
         back to the linear scan"
    );
    assert_test!(
        hdr.eh_frame_ptr == index::eh_frame_addr(),
        "index does not point at the .eh_frame link.ld emitted"
    );
    assert_test!(
        hdr.fde_count >= 1000,
        "index covers implausibly few functions"
    );

    let Some(highest) = index::highest_indexed_function() else {
        slopos_ostd::klog_info!("ASSERT: index has no last entry to probe");
        return TestResult::Fail;
    };
    assert_test!(
        index::enclosing_function(highest + 1) == Some(highest),
        "finder did not resolve the highest indexed function through the table"
    );

    slopos_ostd::klog_info!(
        "unwind index: {} entries, .eh_frame at {:#x}",
        hdr.fde_count,
        hdr.eh_frame_ptr
    );
    TestResult::Pass
}

/// Differential rather than absolute: both catches pay the same fixed cost for
/// the panic handler's backtrace, so the difference is the extra FDE lookups.
/// Dropping `"fde-gnu-eh-frame-hdr"` from the `unwinding` features makes the
/// finder scan linearly, which is what this assert rejects.
pub fn test_unwind_lookup_is_indexed() -> TestResult {
    let (count, limit) = (panic_recovery::oops_count(), panic_recovery::oops_limit());
    // Two deliberate panics must not consume the boot's oops budget.
    panic_recovery::set_oops_limit(0);

    let shallow = time_catch(SHALLOW_DEPTH);
    let deep = time_catch(SHALLOW_DEPTH + DEPTH_DELTA);

    panic_recovery::restore_oops_ledger(count, limit);

    assert_test!(
        shallow > 0 && deep > 0,
        "monotonic clock did not advance across a caught panic"
    );

    let delta = deep.saturating_sub(shallow);
    slopos_ostd::klog_info!(
        "unwind catch: depth {} took {} ns, depth {} took {} ns, delta {} ns",
        SHALLOW_DEPTH,
        shallow,
        SHALLOW_DEPTH + DEPTH_DELTA,
        deep,
        delta
    );
    assert_test!(
        delta < DELTA_BUDGET_NS,
        "per-frame unwind cost is scan-shaped, not index-shaped"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_unwind_index_is_present, suite = unwind_index);
slopos_testing::stest!(name = test_unwind_lookup_is_indexed, suite = unwind_index);
