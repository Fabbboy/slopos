#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod service_cell;
pub mod service_macro;
pub use paste;
pub use service_cell::ServiceCell;
