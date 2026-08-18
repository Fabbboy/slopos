//! `hermetic_state!` function-like macro — one block, one impl, one
//! linker-section vtable entry.
//!
//! ```ignore
//! hermetic_state! {
//!     pub MyState {
//!         type Snapshot = u32;
//!         const DEPENDS_ON: &[&str] = &["OtherState"];   // optional
//!         fn snapshot() -> Result<Self::Snapshot, AllocError> { ... }
//!         fn restore(snap: Self::Snapshot) { ... }
//!     }
//! }
//! ```
//!
//! The user-facing `restore` signature drops the `unsafe fn` token: the macro
//! emits the required `unsafe fn restore(...)` impl item internally, so user
//! bodies and call sites stay free of the `unsafe` keyword.

/// Emits the `.hermetic_state_registry` entry. Consumer crates writing a manual
/// `unsafe impl HermeticState` can call this directly.
#[doc(hidden)]
#[macro_export]
#[allow_internal_unsafe]
macro_rules! __hermetic_register {
    ($ty:ty) => {
        const _: () = {
            $crate::__paste::paste! {
                $crate::registry_entry! {
                    hermetic_states,
                    #[allow(non_upper_case_globals)]
                    static [<__HVT_ $ty>]: $crate::test_support::hermetic::HermeticVTable =
                        $crate::test_support::hermetic::HermeticVTable::new::<$ty>();
                }
            }
        };
    };
}

/// Declare a hermetic-state singleton in one block: marker struct, `unsafe impl
/// HermeticState`, and the `.hermetic_state_registry` linker-section entry.
#[macro_export]
#[allow_internal_unsafe]
macro_rules! hermetic_state {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            type Snapshot = $snap_ty:ty;
            $(const DEPENDS_ON: &[&str] = $deps:expr;)?
            fn snapshot() -> Result<Self::Snapshot, AllocError> $snap_body:block
            fn restore($snap_arg:ident: Self::Snapshot) $rest_body:block
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
