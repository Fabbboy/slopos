//! Userland test-binary registrations.
//!
//! Each [`utest!`](crate::utest) here corresponds to a binary in
//! `userland/src/bin/tests/`. The same binary must also appear in the
//! `test_userland_bins` list in `justfile` so it is packaged into
//! `ext2-tests.img`. If the binary is missing at runtime, the kernel-side
//! [runner](crate::exec::utest::run_thunk) emits a `not ok` line and
//! continues — drift between this file and the build pipeline is detected
//! at harness time, not at compile time.

crate::utest!(
    name = utest_heap_allocator,
    bin = "/bin/heap_allocator_test"
);
crate::utest!(name = utest_fork, bin = "/bin/fork_test");
crate::utest!(name = utest_io_capture, bin = "/bin/io_capture_test");
crate::utest!(
    name = utest_curl_recv_repro,
    bin = "/bin/curl_recv_repro_test"
);
crate::utest!(name = utest_curl_e2e, bin = "/bin/curl_e2e_test");
crate::utest!(name = utest_cd, bin = "/bin/cd_test");
