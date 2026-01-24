//! Safe Context Switch Architecture - Re-export and utilities.
//!
//! The canonical `SwitchContext` struct is defined in `slopos_abi::task`.
//! This module re-exports it and provides offset constants for inline assembly.

pub use slopos_abi::task::{
    SWITCH_CTX_OFF_R12, SWITCH_CTX_OFF_R13, SWITCH_CTX_OFF_R14, SWITCH_CTX_OFF_R15,
    SWITCH_CTX_OFF_RBP, SWITCH_CTX_OFF_RBX, SWITCH_CTX_OFF_RFLAGS, SWITCH_CTX_OFF_RIP,
    SWITCH_CTX_OFF_RSP, SwitchContext,
};

pub const OFF_RBX: usize = SWITCH_CTX_OFF_RBX;
pub const OFF_R12: usize = SWITCH_CTX_OFF_R12;
pub const OFF_R13: usize = SWITCH_CTX_OFF_R13;
pub const OFF_R14: usize = SWITCH_CTX_OFF_R14;
pub const OFF_R15: usize = SWITCH_CTX_OFF_R15;
pub const OFF_RBP: usize = SWITCH_CTX_OFF_RBP;
pub const OFF_RSP: usize = SWITCH_CTX_OFF_RSP;
pub const OFF_RFLAGS: usize = SWITCH_CTX_OFF_RFLAGS;
pub const OFF_RIP: usize = SWITCH_CTX_OFF_RIP;
