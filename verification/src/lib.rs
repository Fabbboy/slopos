//! SlopOS OSTD critical-path verification crate.
//!
//! The proofs are **not** part of this Rust library target: they live as
//! standalone Verus files under `proofs/`, `use vstd::prelude::*` (which
//! resolves only under the pinned Verus toolchain, not the kernel's nightly),
//! and are checked by `just verify`. This `lib.rs` exists only so
//! `verification/` is a first-class workspace member. Per-module status is in
//! `STATUS.md`; `README.md` covers running and upgrading the verifier.

#![no_std]

/// The Verus release this crate's proofs are pinned against — mirrors
/// `verification/verus.toml`'s `version`, duplicated here so tooling need not
/// parse TOML. Update both together.
pub const PINNED_VERUS_VERSION: &str = "0.2026.05.24.ecee80a";

/// The upstream Verus commit the pinned release was cut from. Mirrors the
/// `commit` field of `verification/verus.toml`.
pub const PINNED_VERUS_COMMIT: &str = "ecee80a2139923d503338e6989f79fb690ec7847";
