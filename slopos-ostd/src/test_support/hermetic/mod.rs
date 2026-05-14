//! Hermetic-state framework — snapshot / restore primitives for
//! kernel-singleton state that tests may transiently mutate.
//!
//! The trait and registry vtable live here so the
//! [`crate::hermetic_state`] declarative macro can emit both the
//! `unsafe impl HermeticState` body and the `.hermetic_state_registry`
//! linker-section entry in a single crate without circular deps.
//!
//! The kernel-side walker (`registry_iter`, `topo_order`) and the
//! `KernelTestScope` RAII enter/exit machinery stay in `slopos-hermetic`
//! because they need `KVec` (a kernel-allocator wrapper) and the
//! `pause_all_aps` / `synchronize_rcu` quiescence dance that lives
//! in `slopos-core`.

pub mod macros;
pub mod scope;
pub mod trait_def;
pub mod vtable;

pub use scope::{SnapshotError, run_restore_phase_drain, run_snapshot_phase};
pub use trait_def::HermeticState;
pub use vtable::HermeticVTable;
