//! FFI primitives. Currently hosts the `extern_block!` declarative
//! macro that wraps `unsafe extern "C" { … }` declarations so the
//! `unsafe extern` syntax lives only inside OSTD's macro expansion.
//!
//! # `extern_block!` shape
//!
//! ```ignore
//! slopos_ostd::ffi::extern_block! {
//!     pub mod kernel_syms {
//!         static _text_start: u8;
//!         static _text_end: u8;
//!         #[link_name = "kernel_stack_top"]
//!         static kernel_stack_top_impl: u8;
//!     }
//! }
//! ```
//!
//! expands to a `pub mod kernel_syms` holding one `unsafe extern "C"`
//! block with the three statics declared `pub(super)`, plus three safe
//! accessor functions named `<symbol>_addr() -> *const <ty>`:
//!
//! ```ignore
//! pub mod kernel_syms {
//!     unsafe extern "C" {
//!         pub(super) static _text_start: u8;
//!         pub(super) static _text_end: u8;
//!         #[link_name = "kernel_stack_top"]
//!         pub(super) static kernel_stack_top_impl: u8;
//!     }
//!     pub fn _text_start_addr() -> *const u8 { &raw const _text_start }
//!     pub fn _text_end_addr() -> *const u8 { &raw const _text_end }
//!     pub fn kernel_stack_top_impl_addr() -> *const u8 {
//!         &raw const kernel_stack_top_impl
//!     }
//! }
//! ```
//!
//! `fn` items are consolidated inside the extern block but get **no**
//! safe accessor — callers retain `unsafe { mod_name::fn_name(...) }`
//! at the call site, because whether an external `fn` is safe to call
//! depends on the callee, not on its mere existence as a symbol. The
//! macro's contract is solely to absorb the `unsafe extern` *syntax*.

#[doc(hidden)]
pub use core::ptr;

/// Wrap an `unsafe extern "C" { … }` declaration.
///
/// See the module-level docs for the expansion shape. The macro
/// accepts a mod-wrapped form so multiple invocations in the same
/// scope cannot collide on symbol names — each invocation names its
/// own private module.
///
/// Item kinds supported:
/// - `static NAME: TY;` — emits a safe `pub fn NAME_addr() -> *const TY`
///   accessor at the mod's outer level.
/// - `fn NAME(args) -> RET;` — consolidated inside the extern block;
///   no safe wrapper (callers retain call-site `unsafe { … }`).
///
/// Item-level attributes (`#[link_name = "…"]` on a static, etc.) are
/// preserved verbatim. Outer attributes on the `mod` likewise survive.
#[macro_export]
macro_rules! extern_block {
    (
        $(#[$outer:meta])*
        $vis:vis mod $modname:ident {
            $($body:tt)*
        }
    ) => {
        $(#[$outer])*
        $vis mod $modname {
            unsafe extern "C" {
                $crate::__extern_block_items! { $($body)* }
            }
            $crate::__extern_block_accessors! { $($body)* }
        }
    };
}

/// Internal: emit each item inside the `unsafe extern "C" { … }` block.
///
/// Per-item visibility is forced to `pub(super)` so the safe accessor
/// fns emitted at the outer mod level can reference the extern items.
#[doc(hidden)]
#[macro_export]
macro_rules! __extern_block_items {
    () => {};
    // static form
    (
        $(#[$attr:meta])*
        static $name:ident : $ty:ty ;
        $($rest:tt)*
    ) => {
        $(#[$attr])*
        pub(super) static $name : $ty;
        $crate::__extern_block_items! { $($rest)* }
    };
    // fn form with return type
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) -> $ret:ty ;
        $($rest:tt)*
    ) => {
        $(#[$attr])*
        pub(super) fn $name ( $($args)* ) -> $ret;
        $crate::__extern_block_items! { $($rest)* }
    };
    // fn form without return type
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) ;
        $($rest:tt)*
    ) => {
        $(#[$attr])*
        pub(super) fn $name ( $($args)* );
        $crate::__extern_block_items! { $($rest)* }
    };
}

/// Internal: emit safe accessor fns for each `static` item.
///
/// `fn` items get no accessor — the macro skips them.
#[doc(hidden)]
#[macro_export]
macro_rules! __extern_block_accessors {
    () => {};
    (
        $(#[$attr:meta])*
        static $name:ident : $ty:ty ;
        $($rest:tt)*
    ) => {
        $crate::__paste::paste! {
            #[doc = concat!("Address of extern static `", stringify!($name), "`.")]
            #[allow(non_snake_case)]
            pub fn [<$name _addr>]() -> *const $ty {
                &raw const $name
            }
        }
        $crate::__extern_block_accessors! { $($rest)* }
    };
    // skip fn-with-ret-type
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) -> $ret:ty ;
        $($rest:tt)*
    ) => {
        $crate::__extern_block_accessors! { $($rest)* }
    };
    // skip fn-without-ret-type
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) ;
        $($rest:tt)*
    ) => {
        $crate::__extern_block_accessors! { $($rest)* }
    };
}
