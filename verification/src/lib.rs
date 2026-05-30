//! SlopOS OSTD critical-path verification crate.
//!
//! This crate is the home of the machine-checked proofs of the OSTD
//! critical-path invariants: `Frame<M>` reference counting, slab /
//! `HeapSlot` lifetimes, and `VmSpace::cursor` well-formedness.
//!
//! # Where the proofs live
//!
//! The proofs themselves are **not** part of this Rust library target.
//! They live as standalone Verus files under [`proofs/`] and are checked
//! by the pinned Verus toolchain (`verification/verus.toml`) via
//! `just verify` (`scripts/verify.sh`). They `use vstd::prelude::*`, which
//! only resolves under the Verus toolchain — not the kernel's nightly —
//! so compiling them as a cargo target is neither possible nor desired.
//!
//! This `lib.rs` exists only so `verification/` is a first-class workspace
//! member (`cargo metadata`, `cargo fmt`, and editor tooling all see it).
//!
//! [`proofs/`]: https://github.com/ (see verification/proofs/ in-tree)
//!
//! # Status
//!
//! See [`STATUS.md`](../STATUS.md) for the per-module verification status
//! and [`README.md`](../README.md) for how to run and upgrade the verifier.

#![no_std]

/// The Verus release this crate's proofs are pinned against. Mirrors the
/// `version` field of `verification/verus.toml`; kept here so downstream
/// tooling can read the pin without parsing TOML. Update both together
/// when bumping the toolchain (see `README.md`).
pub const PINNED_VERUS_VERSION: &str = "0.2026.05.24.ecee80a";

/// The upstream Verus commit the pinned release was cut from. Mirrors the
/// `commit` field of `verification/verus.toml`.
pub const PINNED_VERUS_COMMIT: &str = "ecee80a2139923d503338e6989f79fb690ec7847";
