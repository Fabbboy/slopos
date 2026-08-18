//! FFI primitives: the surfaces a `#![forbid(unsafe_code)]` crate cannot
//! spell for itself.
//!
//! Linker registries live in [`registry`]; this module hosts `extern_block!`,
//! the Edition-2024 `no_mangle` wrappers, and the Limine request placement
//! macro.
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
//! `fn` items get **no** safe accessor: whether an external `fn` is safe to
//! call depends on the callee, not on its existence as a symbol.

pub mod registry;

#[doc(hidden)]
pub use core::ptr;

/// Place a static in one of the three sections the Limine boot protocol
/// reads.
///
/// Distinct from [`registry_entry!`](crate::registry_entry): these sections are
/// a bootloader interop contract, hold heterogeneous request types, and nothing
/// walks them from inside the kernel — so no bracket symbols, no entry type.
///
/// ```ignore
/// slopos_ostd::limine_request! {
///     request,
///     static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();
/// }
/// ```
#[macro_export]
#[allow_internal_unsafe]
macro_rules! limine_request {
    (start_marker, $($item:tt)*) => {
        $crate::__limine_request!(".limine_requests_start_marker", $($item)*);
    };
    (request, $($item:tt)*) => {
        $crate::__limine_request!(".limine_requests", $($item)*);
    };
    (end_marker, $($item:tt)*) => {
        $crate::__limine_request!(".limine_requests_end_marker", $($item)*);
    };
}

#[doc(hidden)]
#[macro_export]
#[allow_internal_unsafe]
macro_rules! __limine_request {
    (
        $section:literal,
        $(#[$attr:meta])*
        $vis:vis static $name:ident : $ty:ty = $init:expr ;
    ) => {
        $(#[$attr])*
        #[used]
        #[unsafe(link_section = $section)]
        $vis static $name : $ty = $init;
    };
}

/// Declarative wrapper around `#[unsafe(no_mangle)] pub extern "C" fn …`.
///
/// Edition 2024 spells `no_mangle` as `unsafe(no_mangle)`; the `unsafe` token
/// is syntactic, and this macro absorbs it so consumers never spell it.
///
/// Two forms — with and without a return type:
/// ```ignore
/// slopos_ostd::extern_c_entry! {
///     pub fn kernel_main() { kernel_main_impl() }
/// }
/// slopos_ostd::extern_c_entry! {
///     pub fn isr_iret_frame_corrupt(iret_frame: *const u64) -> ! {
///         handle_corrupt_iret_frame(iret_frame)
///     }
/// }
/// ```
#[macro_export]
#[allow_internal_unsafe]
macro_rules! extern_c_entry {
    (
        $(#[$attr:meta])*
        $vis:vis fn $name:ident ( $($args:tt)* ) -> $ret:ty $body:block
    ) => {
        $(#[$attr])*
        #[unsafe(no_mangle)]
        $vis extern "C" fn $name ( $($args)* ) -> $ret $body
    };
    (
        $(#[$attr:meta])*
        $vis:vis fn $name:ident ( $($args:tt)* ) $body:block
    ) => {
        $(#[$attr])*
        #[unsafe(no_mangle)]
        $vis extern "C" fn $name ( $($args)* ) $body
    };
}

/// Declarative wrapper around `#[unsafe(no_mangle)] $vis static FOO: T = …`.
///
/// For `static` symbols that need the unmangled C-linkage name (e.g. the
/// `SYSCALL_CPU_DATA_PTR` slot consumed by the asm syscall trampoline).
#[macro_export]
#[allow_internal_unsafe]
macro_rules! no_mangle_static {
    (
        $(#[$attr:meta])*
        $vis:vis static $name:ident : $ty:ty = $init:expr ;
    ) => {
        $(#[$attr])*
        #[unsafe(no_mangle)]
        $vis static $name : $ty = $init;
    };
}

/// Wrap an `unsafe extern "C" { … }` declaration.
///
/// See the module-level docs for the expansion shape. The mod-wrapped form
/// keeps multiple invocations in one scope from colliding on symbol names.
///
/// A `static NAME: TY;` gains a safe `pub fn NAME_addr() -> *const TY`; a `fn`
/// gains nothing. Item-level attributes (`#[link_name = "…"]`) survive
/// verbatim.
#[macro_export]
#[allow_internal_unsafe]
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

/// Per-item visibility is forced to `pub(super)` so the safe accessor fns
/// emitted at the outer mod level can reference the extern items.
#[doc(hidden)]
#[macro_export]
macro_rules! __extern_block_items {
    () => {};
    (
        $(#[$attr:meta])*
        static $name:ident : $ty:ty ;
        $($rest:tt)*
    ) => {
        $(#[$attr])*
        pub(super) static $name : $ty;
        $crate::__extern_block_items! { $($rest)* }
    };
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) -> $ret:ty ;
        $($rest:tt)*
    ) => {
        $(#[$attr])*
        pub(super) fn $name ( $($args)* ) -> $ret;
        $crate::__extern_block_items! { $($rest)* }
    };
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
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) -> $ret:ty ;
        $($rest:tt)*
    ) => {
        $crate::__extern_block_accessors! { $($rest)* }
    };
    (
        $(#[$attr:meta])*
        fn $name:ident ( $($args:tt)* ) ;
        $($rest:tt)*
    ) => {
        $crate::__extern_block_accessors! { $($rest)* }
    };
}
