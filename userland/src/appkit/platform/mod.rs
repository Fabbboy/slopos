//! Platform internals: compositor protocol, surfaces, windows.
//!
//! These types are `pub(crate)` — not part of the public AppKit API.
//! The shell uses them directly; widget apps never need to.

pub(crate) mod event;
pub(crate) mod protocol_client;
pub(crate) mod surface;
pub(crate) mod window;
