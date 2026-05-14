//! SlopOS hermetic-state framework.
//!
//! This crate provides three Rust mechanisms that together make
//! "kernel test mutates a singleton without restoring it" a compile-time
//! error or a structurally-impossible runtime condition:
//!
//! 1. The [`HermeticState`] trait + linker-section registry — every
//!    impl is auto-walked at scope enter/Drop. Subsystems with
//!    mutable global state declare a `HermeticState` impl and one
//!    `register_hermetic_state!` macro line; the framework handles
//!    snapshot, topo-sort, and restore.
//! 2. The [`BootCtx`] capability token — boot-time-only mutators
//!    (`gdt_set_ist`, `init_scheduler`, etc.) take `&mut BootCtx` as
//!    an argument. Production code outside the boot path cannot
//!    construct one (the constructor is `pub(crate)`); tests can
//!    only acquire one through `KernelTestScope`.
//! 3. Typed kernel-resource newtypes — [`KernelStackTop<'a>`] (lifetime-
//!    tied stack-top address) and [`slopos_arch::arch::gdt::IstSlot`]
//!    (named enum, no zero / overflow indices). Tests can no longer
//!    pass `0xFFFF_FFFF_8020_0000` as an IST stack top because the
//!    only safe constructor takes a borrowed slice.
//!
//! `KernelTestScope` itself lives in `slopos-core` because it needs
//! `pause_all_aps` and friends from the per-CPU scheduler. It uses the
//! primitives in this crate.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

mod boot_ctx;
mod macros;
mod registry;
mod stack_top;
mod trait_def;

#[doc(hidden)]
pub use paste as __paste;

pub use boot_ctx::{
    clear_test_scope_after_panic, return_after_ap, return_after_boot, return_after_test,
    take_for_ap, take_for_boot, take_for_test, ApInit, BootCtx, BootKind, BspInit, CpuInitKind,
    TestInit,
};
pub use registry::{registry_iter, topo_order, HermeticVTable, RegistryError};
pub use stack_top::KernelStackTop;
pub use trait_def::HermeticState;

// Re-export IstSlot from karch so callers can `use slopos_hermetic::IstSlot`
// without pulling in karch directly. Single source of truth lives in karch.
pub use slopos_arch::arch::gdt::IstSlot;
