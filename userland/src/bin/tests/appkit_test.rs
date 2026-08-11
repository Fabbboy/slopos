#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use slopos_slibc::test_harness::{TestStatus, report};

/// Runs appkit's widget/layout unit tests on the target, where the font atlas
/// and allocator are the ones the GUI apps actually use.
///
/// Reports each case directly rather than going through
/// `test_harness::run`: the suite is a runtime slice of `fn()` that assert,
/// and `run` wants a slice of `fn() -> bool` known at compile time. Wrapping
/// each case in `catch_unwind` keeps one failing assertion from taking the
/// binary down and losing the verdicts of every case after it.
fn main() {
    let mut failed: u32 = 0;

    for &(name, func) in slopos_appkit::tests::cases() {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)).is_ok();
        report(
            if ok {
                TestStatus::Pass
            } else {
                TestStatus::Fail
            },
            name,
            "",
        );
        if !ok {
            failed = failed.saturating_add(1);
        }
    }

    std::process::exit(failed.min(255) as i32);
}
