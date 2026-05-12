//! `HermeticState` trait — lives in `slopos_ostd::test_support::hermetic`.
//!
//! This module is kept as a thin re-export so consumers that still
//! `use slopos_hermetic::HermeticState;` compile unchanged. Prefer the
//! `slopos_ostd::test_support::hermetic::HermeticState` path or the
//! `slopos_ostd::hermetic_state! { ... }` macro for new code.

pub use slopos_ostd::test_support::hermetic::HermeticState;
