//! `hermetic_state!` function-like macro — one block, one impl, one
//! linker-section vtable entry.
//!
//! Subsumes both the `unsafe impl HermeticState for T { ... }` body
//! and the `.hermetic_state_registry` link-section registration that
//! the legacy `register_hermetic_state!(T);` performed separately.
//!
//! ## Macro shape
//!
//! ```ignore
//! hermetic_state! {
//!     pub MyState {
//!         type Snapshot = u32;
//!         const DEPENDS_ON: &[&str] = &["OtherState"];   // optional
//!         fn snapshot() -> Result<Self::Snapshot, AllocError> { ... }
//!         unsafe fn restore(snap: Self::Snapshot) { ... }
//!     }
//! }
//! ```
//!
//! Inspired by `bitflags!` / `pin_project!` — the trait body isn't
//! field-composable (snapshot/restore touch external globals, not
//! struct fields), so a `#[derive]` would have zero callers. A
//! function-like macro covers the boilerplate (struct decl, impl,
//! vtable registration) while leaving the per-impl custom logic
//! to the user.

/// Internal helper that emits the `#[link_section = ".hermetic_state_registry"]`
/// static — the actual linker-section registration. Pulled out as a
/// `macro_rules!` so `hermetic_state!` reuses it (and so consumer
/// crates that want manual `unsafe impl HermeticState` blocks can
/// still call `slopos_ostd::__hermetic_register!(T)` directly).
#[doc(hidden)]
#[macro_export]
macro_rules! __hermetic_register {
    ($ty:ty) => {
        const _: () = {
            $crate::__paste::paste! {
                #[used]
                #[allow(non_upper_case_globals)]
                #[unsafe(link_section = ".hermetic_state_registry")]
                static [<__HVT_ $ty>]: $crate::test_support::hermetic::HermeticVTable =
                    $crate::test_support::hermetic::HermeticVTable::new::<$ty>();
            }
        };
    };
}

/// Declare a hermetic-state singleton in one block.
///
/// Emits the marker struct, the `unsafe impl HermeticState`, and the
/// `.hermetic_state_registry` linker-section entry. See module docs
/// for usage shape.
#[macro_export]
macro_rules! hermetic_state {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            type Snapshot = $snap_ty:ty;
            $(const DEPENDS_ON: &[&str] = $deps:expr;)?
            fn snapshot() -> Result<Self::Snapshot, AllocError> $snap_body:block
            unsafe fn restore($snap_arg:ident: Self::Snapshot) $rest_body:block
        }
    ) => {
        $(#[$meta])*
        $vis struct $name;

        // SAFETY: the macro generates the only impl of `HermeticState`
        // for `$name`; the snapshot/restore bodies are user-provided
        // but the registry-registration / topo-sort framing is
        // generated to match the contract.
        unsafe impl $crate::test_support::hermetic::HermeticState for $name {
            type Snapshot = $snap_ty;
            const NAME: &'static str = ::core::stringify!($name);
            $(const DEPENDS_ON: &'static [&'static str] = $deps;)?
            fn snapshot()
                -> ::core::result::Result<Self::Snapshot, $crate::AllocError>
            $snap_body
            unsafe fn restore($snap_arg: Self::Snapshot) $rest_body
        }

        $crate::__hermetic_register!($name);
    };
}
