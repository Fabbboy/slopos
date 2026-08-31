//! Shared driver-framework core: the bus-agnostic device/driver model
//! ([`bus`]), the `BoundDevice` resource capability, and the interrupt-setup
//! orchestration built on the ostd primitives.

pub mod bound;
pub mod bus;
pub mod msi;
pub mod platform_bound;

pub use bound::BoundError;
pub use bus::{
    Binding, BoundDevice, Bus, ClaimSink, ClaimSlot, ClaimTable, DriverIndex, LinearIndex,
    ProbeError, ProbeOutcome, probe_bus,
};
