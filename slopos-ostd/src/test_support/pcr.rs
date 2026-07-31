//! Safe BSP-PCR snapshot/restore helpers for the hermetic-state framework.
//!
//! The `core/src/scheduler/test_hermetic.rs` impls for `TssIstShadow` and
//! `TssRsp0Shadow` need to read and write a handful of fields on the BSP's
//! Processor Control Region. The raw entry point —
//! `crate::cpu::x86_64::pcr::get_pcr_mut(cpu_id)` — is `unsafe fn` because
//! it returns a `&'static mut ProcessorControlRegion` whose aliasing is the
//! caller's problem. These wrappers absorb that obligation: each helper
//! does the single read/write, returns an owned value (or `Option<owned>`),
//! and never lets a `&mut PCR` escape.
//!
//! # What the caller has to prove, and how
//!
//! An AP never reads the *BSP's* TSS, so the requirement is not the
//! hermetic framework's whole `pause_all_aps` dance — it is narrower and
//! exactly two things, both of which are checked here rather than written
//! down:
//!
//! - **Run on the BSP.** Asserted against `pcr::get_current_cpu()`. Reading
//!   or writing slot 0 from an AP would be a cross-CPU access to another
//!   CPU's live PCR.
//! - **Take no exception mid-update.** The `&IrqDisabled` argument. A
//!   restore rewrites seven IST stack pointers; an interrupt landing
//!   between two of them would dispatch onto a half-updated table.
//!
//! Every helper takes the witness, not just the mutating ones: they share
//! one contract, and a snapshot torn against a concurrent restore is as
//! wrong as a torn restore.

use crate::cpu::x86_64::interrupts::IrqDisabled;
use crate::cpu::x86_64::pcr;

/// Panics unless the caller is the BSP. See the module docs.
#[inline]
fn assert_on_bsp() {
    assert_eq!(
        pcr::get_current_cpu(),
        0,
        "test_support::pcr helpers address the BSP's PCR and must run on the BSP"
    );
}

/// Snapshot the BSP's `tss.ist[0..7]` array.
///
/// Returns `None` if the BSP PCR is not yet initialised (boot-early).
pub fn bsp_ist_snapshot(_irq: &IrqDisabled<'_>) -> Option<[u64; 7]> {
    assert_on_bsp();
    // SAFETY: the BSP PCR is initialised early in kernel_main_impl and remains
    // valid for the kernel's lifetime; `assert_on_bsp` establishes that slot 0
    // is this CPU's own, and the `IrqDisabled` witness keeps the read whole.
    let pcr = unsafe { pcr::get_pcr_mut(0) }?;
    let mut ist = [0u64; 7];
    for i in 0..7 {
        ist[i] = pcr.tss.ist[i];
    }
    Some(ist)
}

/// Restore the BSP's `tss.ist[0..7]` from a snapshot taken by
/// [`bsp_ist_snapshot`]. No-op if the BSP PCR is not initialised.
pub fn bsp_ist_restore(_irq: &IrqDisabled<'_>, snap: [u64; 7]) {
    assert_on_bsp();
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
pub fn bsp_kernel_rsp_snapshot(_irq: &IrqDisabled<'_>) -> Option<u64> {
    assert_on_bsp();
    // SAFETY: see bsp_ist_snapshot.
    let pcr = unsafe { pcr::get_pcr_mut(0) }?;
    Some(pcr.kernel_rsp)
}

/// Restore the BSP's `kernel_rsp` and re-sync `tss.rsp0`. No-op if the BSP
/// PCR is not initialised.
pub fn bsp_kernel_rsp_restore(_irq: &IrqDisabled<'_>, snap: u64) {
    assert_on_bsp();
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
        IrqDisabled::with(|irq| {
            assert!(bsp_ist_snapshot(irq).is_none());
            assert!(bsp_kernel_rsp_snapshot(irq).is_none());
        });
    }

    #[test]
    fn restore_without_pcr_init_is_noop() {
        // Should not panic.
        IrqDisabled::with(|irq| {
            bsp_ist_restore(irq, [0; 7]);
            bsp_kernel_rsp_restore(irq, 0);
        });
    }
}
