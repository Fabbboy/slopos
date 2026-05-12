//! Type-erased vtable for `HermeticState` impls.
//!
//! Each impl emits one of these into the `.hermetic_state_registry`
//! linker section via the [`crate::hermetic_state`] macro. The
//! registry walker in `slopos-hermetic` consumes the resulting
//! contiguous array at scope enter.

use core::ptr::NonNull;

use crate::{AllocError, KBox};

use super::trait_def::HermeticState;

/// Type-erased snapshot/restore vtable entry.
///
/// Layout is intentionally `#[repr(C)]` and pointer-aligned so the
/// linker can KEEP a contiguous array of these, indexed at runtime
/// via the section sentinels `__start_hermetic_state_registry` /
/// `__stop_hermetic_state_registry` declared in `link.ld`.
#[repr(C)]
pub struct HermeticVTable {
    /// Diagnostic name (matches `<S as HermeticState>::NAME`).
    pub name: &'static str,
    /// Dependency list (matches `<S as HermeticState>::DEPENDS_ON`).
    pub depends_on: &'static [&'static str],
    /// Allocate a `KBox<S::Snapshot>` containing the snapshot, return
    /// the leaked raw pointer as `NonNull<()>`. The scope owns the
    /// payload until restore.
    pub snapshot: unsafe fn() -> Result<NonNull<()>, AllocError>,
    /// Consume the payload pointer and invoke `S::restore`. Frees the
    /// `KBox` on completion.
    pub restore: unsafe fn(NonNull<()>),
}

impl HermeticVTable {
    /// Construct a vtable for an `S: HermeticState` impl. Used at
    /// const-eval time by [`crate::hermetic_state`].
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
    // SAFETY: `payload` was produced by `snapshot_thunk::<S>` for the
    // same `S` (registry-vtable invariant: the matching pair is
    // emitted by `hermetic_state! { S { ... } }`).
    let boxed: KBox<S::Snapshot> = unsafe { KBox::from_raw(payload.as_ptr() as *mut S::Snapshot) };
    let snap = KBox::into_inner(boxed);
    // SAFETY: scope contract — only called from KernelTestScope::Drop.
    unsafe { <S as HermeticState>::restore(snap) }
}
