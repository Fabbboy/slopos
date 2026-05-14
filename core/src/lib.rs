#![no_std]
#![feature(sync_unsafe_cell)]
#![forbid(unsafe_code)]

pub mod driver_hooks;
pub mod exec;
pub mod irq;
pub mod scheduler;
#[cfg(feature = "test-hooks")]
pub mod tests;
#[macro_use]
pub mod syscall;

#[cfg(feature = "test-hooks")]
pub mod utests;

/// Register a userland test binary as a `TestDesc` in `.test_registry`.
///
/// The kernel-side runner ([`exec::utest::run_thunk`]) spawns `bin`, blocks
/// until it exits, drains structured per-subtest reports submitted via
/// `SYSCALL_TEST_REPORT`, then emits one parent KTAP line plus one indented
/// subtest line per drained report.
///
/// Lives in `slopos-core` (rather than `slopos-testing`) because the runner
/// needs core-internal APIs (spawn, wait, exit-record, drain). Putting the
/// macro alongside the runner keeps the dep graph one-way: core → testing
/// (for `TestDesc` shape and KTAP emission) and avoids the cycle a
/// testing → core dep would create.
///
/// ```ignore
/// slopos_core::utest!(name = utest_heap_allocator, bin = "/bin/heap_allocator_test");
/// slopos_core::utest!(
///     name = utest_with_args,
///     bin = "/bin/foo",
///     argv = &["foo", "--flag"],
/// );
/// ```
#[macro_export]
macro_rules! utest {
    (name = $ident:ident, bin = $bin:literal) => {
        $crate::utest!(name = $ident, bin = $bin, argv = &[$bin]);
    };

    (name = $ident:ident, bin = $bin:literal, argv = &[$($arg:literal),* $(,)?]) => {
        $crate::__paste::paste! {
            fn [<__utest_thunk_ $ident>]() -> $crate::__testing::TestResult {
                $crate::exec::utest::run_thunk(&[<TEST_DESC_ $ident>])
            }

            $crate::__ostd::link_section_static! {
                #[used]
                #[allow(non_upper_case_globals)]
                section = ".test_registry";
                pub static [<TEST_DESC_ $ident>]: $crate::__testing::TestDesc =
                    $crate::__testing::TestDesc {
                        name: stringify!($ident),
                        module: module_path!(),
                        file: file!(),
                        line: line!(),
                        run: [<__utest_thunk_ $ident>],
                        kind: $crate::__testing::TestKind::Userland,
                        flags: 0,
                        bin: ::core::option::Option::Some($bin),
                        argv: &[$($arg),*],
                    };
            }
        }
    };
}

#[doc(hidden)]
pub use slopos_ostd as __ostd;

#[doc(hidden)]
pub use slopos_testing as __testing;
#[doc(hidden)]
pub use slopos_testing::paste as __paste;

#[cfg(feature = "test-hooks")]
pub use scheduler::context_tests;
pub use scheduler::fate_api;
pub use scheduler::ffi_boundary;
pub use scheduler::kthread;
pub use scheduler::per_cpu;
#[cfg(feature = "test-hooks")]
pub use scheduler::sched_tests;
pub use scheduler::scheduler as sched;
pub use scheduler::task;
pub use scheduler::work_steal;
