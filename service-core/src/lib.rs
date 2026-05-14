#![no_std]
#![forbid(unsafe_code)]

pub mod service_cell;
pub mod service_macro;
pub use paste;
pub use service_cell::ServiceCell;
