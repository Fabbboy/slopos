//! Test-support primitives the kernel-side test harness builds on.
//!
//! Test scaffolding only (`test-hooks`, `#[cfg(test)]`); never production code.

pub mod arch;
pub mod cpu_state;
pub mod gdt;
pub mod global_lock;
pub mod hermetic;
pub mod page_io;
pub mod pcr;
pub mod serial;
pub mod unwind_index;
