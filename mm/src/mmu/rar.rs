//! Shootdown backend abstraction + Intel Remote Action Request (RAR) stub.
//!
//! Intel RAR (Remote Action Request, Sapphire Rapids+) replaces the shootdown
//! IPI with a hardware mailbox: the initiator writes a descriptor into memory
//! and the target CPU processes it asynchronously, even inside a long-latency
//! instruction, with neither side taking an interrupt. `IntelRar::detect`
//! always returns `None` today, so every shootdown runs through `SoftwareIpi`
//! over `mm::tlb`.

use crate::tlb;
use slopos_abi::addr::VirtAddr;

/// A shootdown backend: invalidate a VA (or VA range) on every relevant
/// CPU and block until all of them observe the flush.
pub trait ShootdownBackend: Sync {
    fn flush_page(&self, vaddr: VirtAddr);
    fn flush_range(&self, start: VirtAddr, end: VirtAddr);
    fn flush_all(&self);
    /// Human-readable tag for diagnostics / boot log.
    fn name(&self) -> &'static str;
}

/// IPI-driven shootdown — the current production path.
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

/// Intel RAR backend — detection stub; `flush_*` defer to [`SoftwareIpi`].
pub struct IntelRar;

impl IntelRar {
    /// TODO(tech-debt): always `None` — RAR detection needs the CPUID leaf
    /// 0x2A sub-feature table and the descriptor format, neither of which is
    /// wired up.
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

/// The currently-selected shootdown backend. `&'static dyn` so another
/// backend can be swapped in without revisiting call sites.
pub fn backend() -> &'static dyn ShootdownBackend {
    &SoftwareIpi
}
