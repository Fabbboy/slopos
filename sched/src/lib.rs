#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(unused_imports)]
#![allow(unused_mut)]
#![allow(unused_assignments)]
#![allow(unused_variables)]
#![allow(ambiguous_glob_reexports)]
#![allow(function_casts_as_integer)]

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

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
