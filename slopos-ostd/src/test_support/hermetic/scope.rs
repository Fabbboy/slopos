//! Safe-fn wrappers around the `HermeticVTable::{snapshot,restore}`
//! function pointers, used by `core/src/scheduler/test_fixture.rs`
//! (`KernelTestScope`).
//!
//! The vtable thunks are typed `unsafe fn(...)` because the trait
//! contract is enforced by the framework's "snapshot/restore pair
//! only ever called from `KernelTestScope`" invariant. These wrappers
//! absorb the `unsafe` call: the kernel-side scope code calls
//! `run_snapshot_phase` / `run_restore_phase_drain` and the unsafe
//! token never appears in `test_fixture.rs`.

use core::ptr::NonNull;

use crate::KVec;

use super::vtable::HermeticVTable;

/// Outcome of [`run_snapshot_phase`].
pub enum SnapshotError {
    /// `KVec::push` returned `Err(AllocError)`. The framework rewinds
    /// already-captured snapshots; this variant carries no payload
    /// because there is none to surface.
    Oom,
    /// A specific state's `snapshot()` returned `Err(AllocError)`.
    /// The framework rewinds and reports the name to the caller.
    StateAllocFailed(&'static str),
}

/// Capture every state in `order` into a KVec of `(vtable, payload)`
/// pairs. On failure, the caller is responsible for rewinding any
/// already-captured pairs through [`run_restore_phase_drain`] and
/// reporting via the returned `SnapshotError`.
pub fn run_snapshot_phase(
    order: &[&'static HermeticVTable],
) -> Result<
    KVec<(&'static HermeticVTable, NonNull<()>)>,
    (KVec<(&'static HermeticVTable, NonNull<()>)>, SnapshotError),
> {
    let mut captured: KVec<(&'static HermeticVTable, NonNull<()>)> = KVec::new();
    for vt in order.iter() {
        // SAFETY: registry vtable invariant — `snapshot` is paired with
        // `restore` for the same `S` (the macro emits both from one
        // block). Caller (KernelTestScope::enter) ensures APs paused +
        // BSP execution.
        let snap_result = unsafe { (vt.snapshot)() };
        match snap_result {
            Ok(payload) => {
                if captured.push((*vt, payload)).is_err() {
                    return Err((captured, SnapshotError::Oom));
                }
            }
            Err(_) => {
                return Err((captured, SnapshotError::StateAllocFailed(vt.name)));
            }
        }
    }
    Ok(captured)
}

/// Drain `captured` from the back, invoking each state's `restore`.
/// Used both by `KernelTestScope::Drop` (normal restore) and the
/// unwind paths in `enter()` (snapshot-OOM, init-failure).
///
/// After this returns, `captured` is empty.
pub fn run_restore_phase_drain(captured: &mut KVec<(&'static HermeticVTable, NonNull<()>)>) {
    while let Some((vt, payload)) = captured.pop() {
        // SAFETY: payload was produced by `vt.snapshot()` (registry
        // vtable invariant); caller ensures APs paused + BSP execution.
        unsafe { (vt.restore)(payload) };
    }
}
