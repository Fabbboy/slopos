//! Platform internals: compositor protocol, surfaces, windows.
//!
//! Public API — used by the shell and other low-level apps that need
//! direct access to protocol connections and surfaces.

pub mod event;
pub mod protocol_client;
pub(crate) mod shm;
pub mod surface;
pub(crate) mod sys;
pub mod window;
