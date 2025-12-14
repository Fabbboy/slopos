#![no_std]

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

pub mod ffi_boundary;
pub mod scheduler;
pub mod task;
pub mod kthread;
pub mod test_tasks;
pub mod fate_api;

pub use scheduler::*;
pub use task::*;
pub use kthread::*;
pub use test_tasks::*;
pub use fate_api::*;
