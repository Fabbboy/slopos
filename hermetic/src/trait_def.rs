//! Compatibility re-export of the `HermeticState` trait. New code should use
//! `slopos_ostd::test_support::hermetic::HermeticState` or the
//! `slopos_ostd::hermetic_state! { ... }` macro directly.

pub use slopos_ostd::test_support::hermetic::HermeticState;
