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

/// Declarative wrapper around `#[used] #[unsafe(link_section = "…")]`
/// statics.
///
/// Edition 2024 spells the `link_section` attribute as
/// `#[unsafe(link_section = "…")]` — the `unsafe` keyword is required
/// by the attribute grammar even though the runtime semantics are
/// inert (the linker reads the section label and emplaces the static
/// at the configured offset). This macro absorbs the literal `unsafe`
/// keyword so consumers don't spell it at the registration site.
///
/// Each invocation form:
/// ```ignore
/// slopos_ostd::link_section_static! {
///     #[used]
///     section = ".limine_requests";
///     static FOO: FooT = FooT::new();
/// }
/// ```
/// expands to:
/// ```ignore
/// #[used]
/// #[unsafe(link_section = ".limine_requests")]
/// static FOO: FooT = FooT::new();
/// ```
///
/// `vis` and item-level attributes are forwarded; the visibility
/// defaults to private (`static`) if omitted.
#[macro_export]
macro_rules! link_section_static {
    (
        $(#[$attr:meta])*
        section = $section:literal;
        $vis:vis static $name:ident : $ty:ty = $init:expr ;
    ) => {
        $(#[$attr])*
        #[unsafe(link_section = $section)]
        $vis static $name : $ty = $init;
    };
}

/// Declarative wrapper around `#[unsafe(no_mangle)] pub extern "C" fn …`.
///
/// Edition 2024 marks the `no_mangle` attribute as `unsafe(no_mangle)`
/// at the attribute grammar level. The `unsafe` token is syntactic —
/// the body is plain `pub extern "C" fn`. This macro absorbs the
/// `unsafe` keyword so consumers declare entry points without spelling
/// it.
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
/// Same syntactic-`unsafe`-absorbing role as [`extern_c_entry`], for
/// `static` symbols that need the unmangled C-linkage name (e.g. the
/// `SYSCALL_CPU_DATA_PTR` slot consumed by the asm syscall trampoline).
#[macro_export]
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
