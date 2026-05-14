//! Safe BSP-PCR snapshot/restore helpers for the hermetic-state framework.
//!
//! The `core/src/scheduler/test_hermetic.rs` impls for `TssIstShadow` and
//! `TssRsp0Shadow` need to read and write a handful of fields on the BSP's
//! Processor Control Region. The raw entry point —
//! `crate::cpu::x86_64::pcr::get_pcr_mut(cpu_id)` — is `unsafe fn` because
//! it returns a `&'static mut ProcessorControlRegion` whose aliasing is the
//! caller's problem. These wrappers absorb that obligation: each helper
//! does the single read/write under a documented BSP-only contract, returns
//! an owned value (or `Option<owned>`), and never lets a `&mut PCR`
//! escape.

use crate::cpu::x86_64::pcr;

/// Snapshot the BSP's `tss.ist[0..7]` array.
///
/// Returns `None` if the BSP PCR is not yet initialised (boot-early).
///
/// # Locking / quiescence
/// Hermetic-state framework callers run on BSP under
/// `pause_all_aps + drain_remote_inbox + synchronize_rcu`; APs cannot race.
pub fn bsp_ist_snapshot() -> Option<[u64; 7]> {
    // SAFETY: BSP PCR is initialised early in kernel_main_impl and
    // remains valid for the kernel's lifetime; the snapshot is read-only.
    let pcr = unsafe { pcr::get_pcr_mut(0) }?;
    let mut ist = [0u64; 7];
    for i in 0..7 {
        ist[i] = pcr.tss.ist[i];
    }
    Some(ist)
}

/// Restore the BSP's `tss.ist[0..7]` from a snapshot taken by
/// [`bsp_ist_snapshot`]. No-op if the BSP PCR is not initialised.
///
/// # Safety / quiescence
/// Same as [`bsp_ist_snapshot`]. APs must be paused.
pub fn bsp_ist_restore(snap: [u64; 7]) {
    // SAFETY: see bsp_ist_snapshot.
    if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
        for i in 0..7 {
            pcr.tss.ist[i] = snap[i];
        }
    }
}

/// Snapshot the BSP's `kernel_rsp` field.
///
/// Returns `None` if the BSP PCR is not yet initialised.
pub fn bsp_kernel_rsp_snapshot() -> Option<u64> {
    // SAFETY: see bsp_ist_snapshot.
    let pcr = unsafe { pcr::get_pcr_mut(0) }?;
    Some(pcr.kernel_rsp)
}

/// Restore the BSP's `kernel_rsp` and re-sync `tss.rsp0`. No-op if the BSP
/// PCR is not initialised.
pub fn bsp_kernel_rsp_restore(snap: u64) {
    // SAFETY: see bsp_ist_snapshot.
    if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
        pcr.kernel_rsp = snap;
        pcr.sync_tss_rsp0();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_none_without_pcr_init() {
        // Host-side cargo test runs without the kernel PCR machinery
        // initialised; both helpers must return None rather than UB.
        assert!(bsp_ist_snapshot().is_none());
        assert!(bsp_kernel_rsp_snapshot().is_none());
    }

    #[test]
    fn restore_without_pcr_init_is_noop() {
        // Should not panic.
        bsp_ist_restore([0; 7]);
        bsp_kernel_rsp_restore(0);
    }
}
