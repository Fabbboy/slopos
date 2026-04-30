//! `register_hermetic_state!` macro — emits the linker-section vtable
//! entry for a `HermeticState` impl.
//!
//! Mirrors the `boot_init!` and `stest!` link-section idioms (same
//! `#[link_section]`, same `#[used]`, same `paste::paste!` ident
//! munging). Once the macro is invoked for a type, the framework
//! auto-walks it at scope enter/Drop without further wiring.
//!
//! # Example
//!
//! ```ignore
//! pub struct SleepQueueShadow;
//! unsafe impl HermeticState for SleepQueueShadow {
//!     type Snapshot = KVec<SleepEntry>;
//!     const NAME: &'static str = "SleepQueueShadow";
//!     fn snapshot() -> Result<Self::Snapshot, AllocError> { ... }
//!     unsafe fn restore(snap: Self::Snapshot) { ... }
//! }
//! register_hermetic_state!(SleepQueueShadow);
//! ```

#[macro_export]
macro_rules! register_hermetic_state {
    ($ty:ty) => {
        const _: () = {
            $crate::__paste::paste! {
                #[used]
                #[allow(non_upper_case_globals)]
                #[unsafe(link_section = ".hermetic_state_registry")]
                static [<__HVT_ $ty>]: $crate::HermeticVTable =
                    $crate::HermeticVTable::new::<$ty>();
            }
        };
    };
}
