//! I/O operations — poll, select, and miscellaneous POSIX file ops.

pub mod misc;
pub mod poll;
#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;
