#![feature(restricted_std)]

use slopos_userland as _;

use slopos_slibc::test_harness::{TestStatus, report};

/// Runs appkit's widget/layout unit tests on the target, against the font atlas
/// and allocator the GUI apps actually use.
///
/// Cases are a runtime slice of asserting `fn()`, so `test_harness::run` (which
/// wants compile-time `fn() -> bool`) cannot drive them. Userland is built
/// `panic-strategy = abort`, so a failing case takes the process down: its name
/// goes to stderr before it runs, so the serial log names the case that died.
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
