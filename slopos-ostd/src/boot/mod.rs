//! Boot-time primitives.
//!
//! Pre-SMP one-shot init helpers + AP rendezvous machinery: what the kernel
//! needs to bring CPUs up before any per-CPU runtime service exists.

pub mod handoff;
pub mod hhdm;
pub mod smp;
