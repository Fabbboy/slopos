//! Shootdown backend abstraction + Intel Remote Action Request (RAR) stub.
//!
//! Shootdown has historically been IPI-driven everywhere in SlopOS —
//! see `mm::tlb` for the mature software-IPI path. Intel's RAR (Remote
//! Action Request, Sapphire Rapids+, Xeon 4th Gen) replaces the IPI
//! with a hardware mailbox: the initiator writes a descriptor into
//! memory, the target CPU processes it asynchronously (even inside a
//! long-latency instruction), and neither side takes an interrupt.
//!
//! The trait routes callers through a single dispatch point. The
//! `SoftwareIpi` implementation wraps the existing `mm::tlb` path
//! verbatim. `IntelRar::detect` always returns `None` today — the
//! hardware probe + descriptor layout land when real RAR hardware is
//! plumbed through.

use crate::tlb;
use slopos_abi::addr::VirtAddr;

/// A shootdown backend: invalidate a VA (or VA range) on every relevant
/// CPU and block until all of them observe the flush.
///
/// Implementations are expected to be lock-free on the fast path and
/// stateless from the caller's perspective — concrete backends may
/// cache per-CPU state internally.
pub trait ShootdownBackend: Sync {
    fn flush_page(&self, vaddr: VirtAddr);
    fn flush_range(&self, start: VirtAddr, end: VirtAddr);
    fn flush_all(&self);
    /// Human-readable tag for diagnostics / boot log.
    fn name(&self) -> &'static str;
}

/// IPI-driven shootdown — current production path.
///
/// Delegates to `mm::tlb`, which delivers the Amit-style early ACK +
/// concurrent local flush.
pub struct SoftwareIpi;

impl ShootdownBackend for SoftwareIpi {
    #[inline]
    fn flush_page(&self, vaddr: VirtAddr) {
        tlb::flush_page(vaddr);
    }

    #[inline]
    fn flush_range(&self, start: VirtAddr, end: VirtAddr) {
        tlb::flush_range(start, end);
    }

    #[inline]
    fn flush_all(&self) {
        tlb::flush_all();
    }

    fn name(&self) -> &'static str {
        "software-ipi"
    }
}

/// Intel RAR backend — **detection-stub only**.
///
/// When the hardware probe below returns `Some`, future work will plug
/// in the descriptor-writer, mailbox doorbell, and ack-polling path.
/// Until then, `ShootdownBackend::flush_*` defer to the software IPI
/// layer so correctness is never contingent on the stub.
pub struct IntelRar;

impl IntelRar {
    /// Probe for RAR support via CPUID. Returns `Some(IntelRar)` on
    /// hardware that advertises the feature, `None` otherwise.
    ///
    /// Currently a permanent `None` — RAR detection needs the full
    /// CPUID leaf 0x2A sub-features table and the descriptor format,
    /// neither of which is wired up yet. Keeping this function in
    /// place means every call site already routes through the trait;
    /// wiring RAR on later is a one-line `Box<dyn ShootdownBackend>`
    /// swap.
    pub fn detect() -> Option<Self> {
        None
    }
}

impl ShootdownBackend for IntelRar {
    #[inline]
    fn flush_page(&self, vaddr: VirtAddr) {
        SoftwareIpi.flush_page(vaddr);
    }

    #[inline]
    fn flush_range(&self, start: VirtAddr, end: VirtAddr) {
        SoftwareIpi.flush_range(start, end);
    }

    #[inline]
    fn flush_all(&self) {
        SoftwareIpi.flush_all();
    }

    fn name(&self) -> &'static str {
        "intel-rar"
    }
}

/// The currently-selected shootdown backend.
///
/// Selected once at BSP boot and never swapped thereafter. We keep it a
/// `&'static dyn` — the indirection cost is negligible on the shootdown
/// path (already hundreds of ns for an IPI) and the flexibility gives
/// us a clean drop-in point for RAR later without revisiting every
/// caller.
pub fn backend() -> &'static dyn ShootdownBackend {
    &SoftwareIpi
}
