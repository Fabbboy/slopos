//! Boot-time primitives.
//!
//! Pre-SMP one-shot init helpers + AP rendezvous machinery. Mirrors
//! Asterinas's `ostd::boot` layout — the parts the kernel needs in
//! order to bring CPUs up before any per-CPU runtime services exist.

pub mod handoff;
pub mod hhdm;
pub mod smp;
