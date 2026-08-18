//! Shared driver-framework core: the `BoundDevice` resource capability and the
//! bus-agnostic interrupt-setup orchestration built on the ostd primitives.

pub mod bound;
pub mod msi;
pub mod platform_bound;

pub use bound::{BoundDevice, BoundError};
pub use platform_bound::BoundPlatformDevice;
