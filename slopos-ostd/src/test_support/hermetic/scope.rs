//! Safe-fn wrappers around the `HermeticVTable::{snapshot,restore}`
//! function pointers, so the kernel-side `KernelTestScope` never spells
//! `unsafe`.

use core::ptr::NonNull;

use crate::KVec;

use super::vtable::HermeticVTable;

pub enum SnapshotError {
    /// `KVec::push` returned `Err(AllocError)`.
    Oom,
    /// A specific state's `snapshot()` returned `Err(AllocError)`.
    StateAllocFailed(&'static str),
}

/// Capture every state in `order` into `(vtable, payload)` pairs. On failure
/// the caller must rewind the returned already-captured pairs through
/// [`run_restore_phase_drain`].
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
pub fn run_restore_phase_drain(captured: &mut KVec<(&'static HermeticVTable, NonNull<()>)>) {
    while let Some((vt, payload)) = captured.pop() {
        // SAFETY: payload was produced by `vt.snapshot()` (registry
        // vtable invariant); caller ensures APs paused + BSP execution.
        unsafe { (vt.restore)(payload) };
    }
}
