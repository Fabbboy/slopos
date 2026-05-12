//! Test-support primitives that the kernel-side test harness builds
//! on top of.
//!
//! Only used by test scaffolding (`#[cfg(feature = "test-hooks")]`,
//! `#[cfg(test)]`) — not by production code. The crate-root namespace
//! `slopos_ostd::test_support` keeps these visually separated from
//! the kernel runtime APIs even though they share the same crate.

pub mod hermetic;
