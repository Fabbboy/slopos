//! Type-erased vtable for `HermeticState` impls: [`crate::hermetic_state`]
//! emits one per impl into `.hermetic_state_registry`, which the registry
//! walker in `slopos-hermetic` consumes at scope enter.

use core::ptr::NonNull;

use crate::{AllocError, KBox};

use super::trait_def::HermeticState;

/// Type-erased snapshot/restore vtable entry.
///
/// `#[repr(C)]` and pointer-aligned so the linker can KEEP a contiguous array
/// of these, indexed via the `__start_hermetic_state_registry` /
/// `__stop_hermetic_state_registry` sentinels declared in `link.ld`.
#[repr(C)]
pub struct HermeticVTable {
    pub name: &'static str,
    pub depends_on: &'static [&'static str],
    /// Leaks a `KBox<S::Snapshot>`; the scope owns the payload until restore.
    pub snapshot: unsafe fn() -> Result<NonNull<()>, AllocError>,
    /// Consumes the payload pointer, invokes `S::restore`, frees the `KBox`.
    pub restore: unsafe fn(NonNull<()>),
}

impl HermeticVTable {
    pub const fn new<S: HermeticState>() -> Self {
        Self {
            name: <S as HermeticState>::NAME,
            depends_on: <S as HermeticState>::DEPENDS_ON,
            snapshot: snapshot_thunk::<S>,
            restore: restore_thunk::<S>,
        }
    }
}

unsafe fn snapshot_thunk<S: HermeticState>() -> Result<NonNull<()>, AllocError> {
    let snap = <S as HermeticState>::snapshot()?;
    let boxed = KBox::try_new(snap)?;
    let raw = KBox::into_raw(boxed) as *mut ();
    // SAFETY: `KBox::into_raw` returns a non-null pointer.
    Ok(unsafe { NonNull::new_unchecked(raw) })
}

unsafe fn restore_thunk<S: HermeticState>(payload: NonNull<()>) {
    // SAFETY: `payload` came from `snapshot_thunk::<S>` for the same `S` —
    // `hermetic_state! { S { ... } }` emits the pair together.
    let boxed: KBox<S::Snapshot> = unsafe { KBox::from_raw(payload.as_ptr() as *mut S::Snapshot) };
    let snap = KBox::into_inner(boxed);
    // SAFETY: scope contract — only called from KernelTestScope::Drop.
    unsafe { <S as HermeticState>::restore(snap) }
}

impl crate::ffi::registry::RegistryEntry for HermeticVTable {
    const REGISTRIES: &'static [crate::ffi::registry::RegistryId] =
        &[crate::ffi::registry::RegistryId::HermeticStates];
}
