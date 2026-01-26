//! Re-export of SwitchContext from slopos_abi.
//!
//! The canonical `SwitchContext` and `SwitchContextBuilder` are defined in `slopos_abi::task`.
//! Assembly code in switch_asm.rs uses `offset_of!()` directly for compile-time safety.

pub use slopos_abi::task::{SwitchContext, SwitchContextBuilder};
