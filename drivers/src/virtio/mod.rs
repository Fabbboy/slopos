//! Shared types, constants and helpers for the VirtIO device drivers.

pub mod pci;
pub mod queue;

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_mm::mmio::MmioRegion;
use slopos_ostd::lock_class;
use slopos_ostd::sync::WaitAbort;
use slopos_ostd::sync::WaitQueue;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 0x01;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 0x02;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 0x04;

/// OS has found the device.
pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
/// OS knows how to drive the device.
pub const VIRTIO_STATUS_DRIVER: u8 = 0x02;
/// Driver is ready to drive the device.
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 0x04;
/// Feature negotiation complete.
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 0x08;
/// Something went wrong; the device must be reset.
pub const VIRTIO_STATUS_FAILED: u8 = 0x80;

/// VirtIO 1.0+ compliant device
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Descriptor continues via the `next` field
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Buffer is device-writable (vs device-readable)
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Buffer contains a list of buffer descriptors
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// VirtIO MSI-X "no vector" sentinel (§4.1.4.3): written to `queue_msix_vector`
/// or `msix_config` it disables MSI-X delivery for that queue or for
/// configuration-change notifications.
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

/// Maximum number of virtqueues tracked for per-queue MSI-X vectors.
pub const MAX_MSIX_QUEUES: usize = 4;

pub use crate::pci_defs::{
    PCI_CAP_ID_VNDR, PCI_CAP_PTR_OFFSET, PCI_STATUS_CAP_LIST, PCI_STATUS_OFFSET,
};

pub const COMMON_CFG_DEVICE_FEATURE_SELECT: usize = 0x00;
pub const COMMON_CFG_DEVICE_FEATURE: usize = 0x04;
pub const COMMON_CFG_DRIVER_FEATURE_SELECT: usize = 0x08;
pub const COMMON_CFG_DRIVER_FEATURE: usize = 0x0C;
pub const COMMON_CFG_MSIX_CONFIG: usize = 0x10;
pub const COMMON_CFG_NUM_QUEUES: usize = 0x12;
pub const COMMON_CFG_CONFIG_GENERATION: usize = 0x15;
pub const COMMON_CFG_QUEUE_MSIX_VECTOR: usize = 0x1A;
pub const COMMON_CFG_DEVICE_STATUS: usize = 0x14;
pub const COMMON_CFG_QUEUE_SELECT: usize = 0x16;
pub const COMMON_CFG_QUEUE_SIZE: usize = 0x18;
pub const COMMON_CFG_QUEUE_ENABLE: usize = 0x1C;
pub const COMMON_CFG_QUEUE_NOTIFY_OFF: usize = 0x1E;
/// Low half of the 64-bit queue_desc address.
pub const COMMON_CFG_QUEUE_DESC: usize = 0x20;
/// Low half of the 64-bit queue_avail address.
pub const COMMON_CFG_QUEUE_AVAIL: usize = 0x28;
/// Low half of the 64-bit queue_used address.
pub const COMMON_CFG_QUEUE_USED: usize = 0x30;

/// Parsed VirtIO PCI capabilities - MMIO regions for device interaction
#[derive(Clone)]
pub struct VirtioMmioCaps {
    pub common_cfg: MmioRegion,
    pub notify_cfg: MmioRegion,
    pub notify_off_multiplier: u32,
    pub device_cfg: MmioRegion,
    pub device_cfg_len: u32,
}

impl VirtioMmioCaps {
    pub const fn empty() -> Self {
        Self {
            common_cfg: MmioRegion::empty(),
            notify_cfg: MmioRegion::empty(),
            notify_off_multiplier: 0,
            device_cfg: MmioRegion::empty(),
            device_cfg_len: 0,
        }
    }

    #[inline]
    pub fn has_common_cfg(&self) -> bool {
        self.common_cfg.is_mapped()
    }

    #[inline]
    pub fn has_notify_cfg(&self) -> bool {
        self.notify_cfg.is_mapped()
    }

    #[inline]
    pub fn has_device_cfg(&self) -> bool {
        self.device_cfg.is_mapped()
    }
}

/// HPET-based polling loop. On the BSP `sti; hlt` sleeps until the next
/// interrupt; APs spin. Returns `false` on timeout.
pub(crate) fn hpet_poll_wait(condition: &dyn Fn() -> bool, timeout_ms: u32) -> bool {
    use crate::hpet;

    if condition() {
        return true;
    }

    let Some(ticks_needed) = hpet::ms_to_ticks(timeout_ms) else {
        for _ in 0..100_000u32 {
            if condition() {
                return true;
            }
            core::hint::spin_loop();
        }
        return condition();
    };

    let start = hpet::read_counter();
    let allow_hlt = slopos_arch::pcr::get_current_cpu() == 0;

    loop {
        slopos_ostd::cpu::x86_64::interrupts::disable_interrupts();

        if condition() {
            slopos_ostd::cpu::x86_64::interrupts::enable_interrupts();
            return true;
        }

        if hpet::read_counter().wrapping_sub(start) >= ticks_needed {
            slopos_ostd::cpu::x86_64::interrupts::enable_interrupts();
            return false;
        }

        if allow_hlt {
            slopos_ostd::cpu::x86_64::core::sti_hlt_atomic();
        } else {
            slopos_ostd::cpu::x86_64::interrupts::enable_interrupts();
            core::hint::spin_loop();
        }
    }
}

/// Edge-triggered event signalled from IRQ context, waited on by tasks.
///
/// The edge is latched in `signaled`, so a signal that fires while no waiter
/// is parked is consumed by the next wait. Before the scheduler backend is
/// registered (device probe, early boot), waits degrade to the HPET
/// cli/sti/hlt poll loop.
pub struct IrqEdgeEvent {
    signaled: AtomicBool,
    waiters: WaitQueue,
}

impl IrqEdgeEvent {
    pub const fn new() -> Self {
        Self {
            signaled: AtomicBool::new(false),
            waiters: WaitQueue::new(lock_class!("IrqEdgeEvent.waiters", LOCK_LEVEL_RESOURCE)),
        }
    }

    /// **IRQ-safe.** Latch the edge and wake every parked waiter.
    #[inline]
    pub fn signal(&self) {
        self.signaled.store(true, Ordering::Release);
        let _ = self.waiters.wake_all();
    }

    #[inline]
    pub fn reset(&self) {
        self.signaled.store(false, Ordering::Release);
    }

    #[inline]
    pub fn try_consume(&self) -> bool {
        self.signaled
            .compare_exchange(true, false, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Park until the edge fires or `timeout_ms` elapses. Returns `true`
    /// iff the edge was consumed.
    #[inline]
    pub fn wait_timeout_ms(&self, timeout_ms: u32) -> bool {
        matches!(
            self.wait_timeout(timeout_ms),
            EdgeWait::Latched | EdgeWait::Woken
        )
    }

    /// [`wait_timeout_ms`](Self::wait_timeout_ms), reporting which way the wait
    /// ended rather than only whether it was satisfied.
    ///
    /// A timeout never expires before `timeout_ms` has elapsed: the wait
    /// queue's deadline comes from a millisecond clock that truncates, so the
    /// budget handed to it carries the partial millisecond it would otherwise
    /// drop.
    pub fn wait_timeout(&self, timeout_ms: u32) -> EdgeWait {
        if self.try_consume() {
            return EdgeWait::Latched;
        }

        match self.waiters.wait_event_timeout_until(
            || if self.try_consume() { Some(()) } else { None },
            timeout_ms as u64 + 1,
        ) {
            Ok(()) => EdgeWait::Woken,
            Err(WaitAbort::Timeout) => EdgeWait::TimedOut,
            // Pre-scheduler context (probe paths): fall back to polling.
            Err(WaitAbort::NoRuntime) => {
                if hpet_poll_wait(&|| self.try_consume(), timeout_ms) {
                    EdgeWait::Woken
                } else {
                    EdgeWait::TimedOut
                }
            }
            Err(_) => EdgeWait::Aborted,
        }
    }
}

/// How an [`IrqEdgeEvent`] wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeWait {
    /// The edge was already latched and was consumed without parking.
    Latched,
    /// Parked, then woken by the edge.
    Woken,
    /// The deadline passed with no edge.
    TimedOut,
    /// The waiting task was killed or took a signal.
    Aborted,
}

/// Legacy alias — use [`IrqEdgeEvent`] for new code.
pub type QueueEvent = IrqEdgeEvent;

/// Active interrupt delivery mechanism for a VirtIO device. At least MSI is
/// required — legacy polling is not supported, and probe panics if neither
/// MSI-X nor MSI can be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMode {
    /// MSI: single shared vector for all queues.
    Msi {
        /// Allocated IDT vector (48–223).
        vector: u8,
    },
    /// MSI-X: per-queue vectors via the MSI-X table.
    Msix { num_queues: u8 },
}

/// Per-device MSI-X state produced by [`pci::setup_interrupts`].
///
/// Callers keep it alive for the device lifetime: it owns the table's MMIO
/// mapping. The vectors' owned IRQ bindings live in the device's resource bag,
/// so this stays `Clone`.
#[derive(Clone)]
pub struct VirtioMsixState {
    pub cap: crate::msix::MsixCapability,
    pub table: crate::msix::MsixTable,
    /// Allocated IDT vector for each queue (0 = not assigned).
    pub queue_vectors: [u8; MAX_MSIX_QUEUES],
    pub num_queues: u8,
}

impl VirtioMsixState {
    /// MSI-X table entry for `queue_idx`: entries 0..N-1 map to queues 0..N-1,
    /// with no config-change entry. [`VIRTIO_MSI_NO_VECTOR`] if it has none.
    #[inline]
    pub fn queue_msix_entry(&self, queue_idx: u16) -> u16 {
        let i = queue_idx as usize;
        if i < self.num_queues as usize && self.queue_vectors[i] != 0 {
            queue_idx
        } else {
            VIRTIO_MSI_NO_VECTOR
        }
    }

    #[inline]
    pub fn queue_idt_vector(&self, queue_idx: u16) -> Option<u8> {
        let i = queue_idx as usize;
        if i < self.num_queues as usize && self.queue_vectors[i] != 0 {
            Some(self.queue_vectors[i])
        } else {
            None
        }
    }
}

#[inline]
pub fn set_device_status(cfg: &MmioRegion, status: u8) {
    cfg.write::<u8>(COMMON_CFG_DEVICE_STATUS, status);
}

#[inline]
pub fn get_device_status(cfg: &MmioRegion) -> u8 {
    cfg.read::<u8>(COMMON_CFG_DEVICE_STATUS)
}

#[inline]
pub fn reset_device(cfg: &MmioRegion) {
    set_device_status(cfg, 0);
}

/// VirtIO write memory barrier: per spec 2.7.7, descriptor writes must be
/// visible before avail idx is updated.
#[inline(always)]
pub fn virtio_wmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}

/// VirtIO read memory barrier: per spec 2.7.13, used idx must be observed
/// before completion data is read.
#[inline(always)]
pub fn virtio_rmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
}
