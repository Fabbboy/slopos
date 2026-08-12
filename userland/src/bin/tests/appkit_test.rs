#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use slopos_slibc::test_harness::{TestStatus, report};

/// Runs appkit's widget/layout unit tests on the target, where the font atlas
/// and allocator are the ones the GUI apps actually use.
///
/// Reports each case directly rather than going through `test_harness::run`:
/// the suite is a runtime slice of `fn()` that assert, and `run` wants a slice
/// of `fn() -> bool` known at compile time.
///
/// A failing case takes the process down rather than being caught: userland is
/// built `panic-strategy = abort`, so `catch_unwind` cannot recover here. The
/// case name therefore goes to stderr *before* it runs, so the serial log
/// names the case that died, and the kernel-side runner fails the parent on
/// the abnormal exit. A report can only be appended, never amended, so
/// reporting a provisional verdict up front is not an option.
fn main() {
    let cases = slopos_appkit::tests::cases();
    let mut passed = 0usize;

    for &(name, func) in cases {
        eprintln!("appkit_test: running {name}");
        func();
        report(TestStatus::Pass, name, "");
        passed += 1;
    }

    eprintln!("appkit_test: {passed}/{} cases passed", cases.len());
    std::process::exit(0);
}
