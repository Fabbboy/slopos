//! Shared driver-framework core: the `BoundDevice` resource capability and the
//! bus-agnostic interrupt-setup orchestration that drivers build on top of the
//! ostd primitives. The future home for shared registry / bus seams.

pub mod bound;
pub mod msi;

pub use bound::{BoundDevice, BoundError};
