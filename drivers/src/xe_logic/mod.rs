//! Pure logic for the Intel xe display driver: plain functions over plain data,
//! no MMIO and no ostd device primitives. Split out from the hardware-sequencing
//! half in [`crate::xe`] so it stays testable under `just test`, which cannot
//! emulate the GPU.

pub mod cmdline;
pub mod cursor_config;
pub mod ddb;
pub mod ggtt_pte;
pub mod plane_config;
pub mod platform;
pub mod regs;
