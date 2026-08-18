//! SlopOS hermetic-state framework: the [`HermeticState`] linker-section
//! registry walked at scope enter/Drop, the [`BootCtx`] capability token
//! gating boot-time-only mutators, and the [`KernelStackTop<'a>`] and
//! [`slopos_arch::arch::gdt::IstSlot`] resource newtypes.
//!
//! `KernelTestScope` itself lives in `slopos-core` and builds on these
//! primitives.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

mod boot_ctx;
mod registry;
mod stack_top;
mod trait_def;

#[doc(hidden)]
pub use paste as __paste;

pub use boot_ctx::{
    ApInit, BootCtx, BootKind, BspInit, CpuInitKind, TestInit, clear_test_scope_after_panic,
    return_after_ap, return_after_boot, return_after_test, take_for_ap, take_for_boot,
    take_for_test,
};
pub use registry::{HermeticVTable, RegistryError, registry_iter, topo_order};
pub use stack_top::KernelStackTop;
pub use trait_def::HermeticState;

pub use slopos_arch::arch::gdt::IstSlot;
