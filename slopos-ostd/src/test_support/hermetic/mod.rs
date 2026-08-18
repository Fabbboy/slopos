//! Hermetic-state framework — snapshot / restore primitives for
//! kernel-singleton state that tests may transiently mutate.
//!
//! The trait and registry vtable live here so [`crate::hermetic_state`] can emit
//! both the `unsafe impl` and the `.hermetic_state_registry` entry without a
//! circular dep. The registry walker and `KernelTestScope` stay in
//! `slopos-hermetic`, which has `KVec` and the `slopos-core` quiescence dance.

pub mod macros;
pub mod scope;
pub mod trait_def;
pub mod vtable;

pub use scope::{SnapshotError, run_restore_phase_drain, run_snapshot_phase};
pub use trait_def::HermeticState;
pub use vtable::HermeticVTable;
