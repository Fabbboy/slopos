//! Safe BSP-PCR snapshot/restore helpers for the hermetic-state framework.
//!
//! Every helper asserts it runs on the BSP and takes an `&IrqDisabled`
//! witness: a restore rewrites seven IST stack pointers, and an interrupt
//! landing between two of them would dispatch onto a half-updated table.

use crate::cpu::x86_64::interrupts::IrqDisabled;
use crate::cpu::x86_64::pcr;

#[inline]
fn assert_on_bsp() {
    assert_eq!(
        pcr::get_current_cpu(),
        0,
        "test_support::pcr helpers address the BSP's PCR and must run on the BSP"
    );
}

/// Returns `None` if the BSP PCR is not yet initialised.
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

/// Restores a snapshot from [`bsp_ist_snapshot`]. No-op if the BSP PCR is not
/// initialised.
pub fn bsp_ist_restore(_irq: &IrqDisabled<'_>, snap: [u64; 7]) {
    assert_on_bsp();
    // SAFETY: see bsp_ist_snapshot.
    if let Some(pcr) = unsafe { pcr::get_pcr_mut(0) } {
        for i in 0..7 {
            pcr.tss.ist[i] = snap[i];
        }
    }
}

/// Returns `None` if the BSP PCR is not yet initialised.
pub fn bsp_kernel_rsp_snapshot(_irq: &IrqDisabled<'_>) -> Option<u64> {
    assert_on_bsp();
    // SAFETY: see bsp_ist_snapshot.
    let pcr = unsafe { pcr::get_pcr_mut(0) }?;
    Some(pcr.kernel_rsp)
}

/// Also re-syncs `tss.rsp0`. No-op if the BSP PCR is not initialised.
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
        IrqDisabled::with(|irq| {
            assert!(bsp_ist_snapshot(irq).is_none());
            assert!(bsp_kernel_rsp_snapshot(irq).is_none());
        });
    }

    #[test]
    fn restore_without_pcr_init_is_noop() {
        IrqDisabled::with(|irq| {
            bsp_ist_restore(irq, [0; 7]);
            bsp_kernel_rsp_restore(irq, 0);
        });
    }
}
