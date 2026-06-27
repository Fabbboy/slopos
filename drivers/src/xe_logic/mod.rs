//! Pure, host/`stest!`-testable logic for the Intel xe display driver.
//!
//! Everything here is plain functions over plain data: no MMIO, no allocation,
//! no scanout arbiter, no `slopos-ostd` device primitives. The whole xe driver
//! is compiled into every kernel; this pure half is split out so its regression
//! tests run in the standard `just test` suite (host/`stest!`-testable) even
//! though the GPU itself cannot be emulated under QEMU. The hardware-sequencing
//! half lives in the sibling [`crate::xe`] module and drives these functions.

pub mod cmdline;
pub mod cursor_config;
pub mod ddb;
pub mod ggtt_pte;
pub mod plane_config;
pub mod platform;
pub mod regs;
