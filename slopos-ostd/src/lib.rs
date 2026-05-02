//! SlopOS Operating-System Trusted Domain (OSTD).
//!
//! This crate is the kernel's trusted core: every line of `unsafe` in
//! the kernel will eventually live here. All other kernel crates
//! consume safe APIs exposed by this crate. Phase 1A is the empty
//! skeleton — modules are populated incrementally in 1B through 1I.
//! See `plans/FRAMEKERNEL_PLAN.md`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod arch;
pub mod boot;
pub mod cpu;
pub mod io;
pub mod irq;
pub mod mm;
pub mod sync;
pub mod task;
pub mod user;
