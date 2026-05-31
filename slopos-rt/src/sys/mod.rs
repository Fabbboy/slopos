//! Thin syscall shims `slopfut` + the `Ring` wrapper need. Copied verbatim
//! from `userland/src/syscall/`, retargeted onto `slopos_slibc::pal::raw`.

pub mod memory;
pub mod pidfd;
pub mod process;
pub mod ring;
pub mod signalfd;
