//! VirtIO common infrastructure
//!
//! This module provides shared types, constants, and utilities for VirtIO device drivers.
//! It eliminates code duplication between virtio-blk and future virtio drivers.

pub mod pci;
pub mod queue;

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_mm::mmio::MmioRegion;
use slopos_ostd::sync::WaitAbort;
use slopos_ostd::sync::WaitQueue;

// =============================================================================
// VirtIO PCI Capability Types
// =============================================================================

/// VirtIO PCI capability type: Common configuration
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 0x01;
/// VirtIO PCI capability type: Notification area
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 0x02;
/// VirtIO PCI capability type: Device-specific configuration
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 0x04;

// =============================================================================
// VirtIO Device Status Bits
// =============================================================================

/// Device status: OS has found the device
pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 0x01;
/// Device status: OS knows how to drive the device
pub const VIRTIO_STATUS_DRIVER: u8 = 0x02;
/// Device status: Driver is ready to drive the device
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 0x04;
/// Device status: Feature negotiation complete
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 0x08;
/// Device status: Something went wrong (device should be reset)
pub const VIRTIO_STATUS_FAILED: u8 = 0x80;

// =============================================================================
// VirtIO Feature Bits
// =============================================================================

/// VirtIO 1.0+ compliant device
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// =============================================================================
// VirtIO Queue Descriptor Flags
// =============================================================================

/// Descriptor continues via the `next` field
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Buffer is device-writable (vs device-readable)
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Buffer contains a list of buffer descriptors
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// VirtIO MSI-X "no vector" sentinel (§4.1.4.3).
///
/// Writing this to `queue_msix_vector` or `msix_config` disables MSI-X
/// delivery for the respective queue or configuration change notification.
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

/// Maximum number of virtqueues tracked for per-queue MSI-X vectors.
pub const MAX_MSIX_QUEUES: usize = 4;

pub use crate::pci_defs::{
    PCI_CAP_ID_VNDR, PCI_CAP_PTR_OFFSET, PCI_STATUS_CAP_LIST, PCI_STATUS_OFFSET,
};

// =============================================================================
// VirtIO Common Configuration Layout (MMIO offsets)
// =============================================================================

/// Offset to device_feature_select in common config
pub const COMMON_CFG_DEVICE_FEATURE_SELECT: usize = 0x00;
/// Offset to device_feature in common config
pub const COMMON_CFG_DEVICE_FEATURE: usize = 0x04;
/// Offset to driver_feature_select in common config
pub const COMMON_CFG_DRIVER_FEATURE_SELECT: usize = 0x08;
/// Offset to driver_feature in common config
pub const COMMON_CFG_DRIVER_FEATURE: usize = 0x0C;
/// Offset to msix_config in common config (configuration change MSI-X vector)
pub const COMMON_CFG_MSIX_CONFIG: usize = 0x10;
/// Offset to num_queues in common config
pub const COMMON_CFG_NUM_QUEUES: usize = 0x12;
/// Offset to config_generation in common config
pub const COMMON_CFG_CONFIG_GENERATION: usize = 0x15;
/// Offset to queue_msix_vector in common config (per-queue MSI-X vector)
pub const COMMON_CFG_QUEUE_MSIX_VECTOR: usize = 0x1A;
/// Offset to device_status in common config
pub const COMMON_CFG_DEVICE_STATUS: usize = 0x14;
/// Offset to queue_select in common config
pub const COMMON_CFG_QUEUE_SELECT: usize = 0x16;
/// Offset to queue_size in common config
pub const COMMON_CFG_QUEUE_SIZE: usize = 0x18;
/// Offset to queue_enable in common config
pub const COMMON_CFG_QUEUE_ENABLE: usize = 0x1C;
/// Offset to queue_notify_off in common config
pub const COMMON_CFG_QUEUE_NOTIFY_OFF: usize = 0x1E;
/// Offset to queue_desc (low) in common config
pub const COMMON_CFG_QUEUE_DESC: usize = 0x20;
/// Offset to queue_avail (low) in common config
pub const COMMON_CFG_QUEUE_AVAIL: usize = 0x28;
/// Offset to queue_used (low) in common config
pub const COMMON_CFG_QUEUE_USED: usize = 0x30;

// =============================================================================
// VirtIO MMIO Capabilities
// =============================================================================

/// Parsed VirtIO PCI capabilities - MMIO regions for device interaction
#[derive(Clone)]
pub struct VirtioMmioCaps {
    /// Common configuration region
    pub common_cfg: MmioRegion,
    /// Notification region
    pub notify_cfg: MmioRegion,
    /// Notify offset multiplier (from PCI cap)
    pub notify_off_multiplier: u32,
    /// Device-specific configuration region
    pub device_cfg: MmioRegion,
    /// Length of device config region
    pub device_cfg_len: u32,
}

impl VirtioMmioCaps {
    /// Create empty capabilities (no regions mapped)
    pub const fn empty() -> Self {
        Self {
            common_cfg: MmioRegion::empty(),
            notify_cfg: MmioRegion::empty(),
            notify_off_multiplier: 0,
            device_cfg: MmioRegion::empty(),
            device_cfg_len: 0,
        }
    }

    /// Check if common config is available
    #[inline]
    pub fn has_common_cfg(&self) -> bool {
        self.common_cfg.is_mapped()
    }

    /// Check if notify config is available
    #[inline]
    pub fn has_notify_cfg(&self) -> bool {
        self.notify_cfg.is_mapped()
    }

    /// Check if device config is available
    #[inline]
    pub fn has_device_cfg(&self) -> bool {
        self.device_cfg.is_mapped()
    }
}

// =============================================================================
// Shared HPET helpers
// =============================================================================

/// HPET-based polling loop with cli/sti/hlt for efficient IRQ-driven wakeup.
///
/// Checks `condition` each iteration. On BSP (CPU 0) uses `sti; hlt` to
/// sleep until the next interrupt; on APs falls back to spin_loop.
/// Returns `true` if `condition` returned `true`, `false` on timeout.
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

// =============================================================================
// IrqEdgeEvent — scheduler-backed edge-triggered IRQ event
// =============================================================================

/// Edge-triggered event signalled from IRQ context, waited on by tasks.
///
/// The producer side ([`signal`](Self::signal)) is a `Release` store plus
/// `WaitQueue::wake_all` — IRQ-safe by the wait-queue contract ("IRQ
/// context for `wake_*`"). The consumer side parks scheduler-backed via
/// `wait_event_timeout_until`, so a waiting task deschedules and frees
/// its CPU instead of HLT-polling; the wait queue's enqueue-then-recheck
/// protocol closes the lost-wakeup window. The edge is latched in
/// `signaled`, so a signal that fires while no waiter is parked is
/// consumed by the next wait (single-token `parking`-style semantics).
///
/// Before the scheduler backend is registered (device probe, early
/// boot), waits degrade to the HPET cli/sti/hlt poll loop — the only
/// context where burning the CPU on a wait is acceptable.
pub struct IrqEdgeEvent {
    signaled: AtomicBool,
    /// Tasks park here; `signal()` wakes from IRQ context.
    waiters: WaitQueue,
}

impl IrqEdgeEvent {
    pub const fn new() -> Self {
        Self {
            signaled: AtomicBool::new(false),
            waiters: WaitQueue::new(),
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
    pub fn wait_timeout_ms(&self, timeout_ms: u32) -> bool {
        match self.waiters.wait_event_timeout_until(
            || if self.try_consume() { Some(()) } else { None },
            timeout_ms as u64,
        ) {
            Ok(()) => true,
            // Pre-scheduler context (probe paths): fall back to polling.
            Err(WaitAbort::NoRuntime) => hpet_poll_wait(&|| self.try_consume(), timeout_ms),
            Err(_) => false,
        }
    }
}

/// Legacy alias — use [`IrqEdgeEvent`] for new code.
pub type QueueEvent = IrqEdgeEvent;

// =============================================================================
// VirtIO Interrupt Mode
// =============================================================================

/// Active interrupt delivery mechanism for a VirtIO device.
///
/// VirtIO modern devices on QEMU q35 always expose MSI-X.  The kernel
/// requires at least MSI as a fallback; legacy polling is not supported.
/// Probe will panic if neither MSI-X nor MSI can be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMode {
    /// MSI: single shared vector for all queues.
    Msi {
        /// Allocated IDT vector (48–223).
        vector: u8,
    },
    /// MSI-X: per-queue vectors via the MSI-X table.
    Msix {
        /// Number of queues with assigned MSI-X entries.
        num_queues: u8,
    },
}

/// Per-device MSI-X state produced by [`pci::setup_interrupts`].
///
/// Stores the mapped MSI-X table, the allocated IDT vector numbers for each
/// virtqueue, and the capability. Callers keep this alive for the device
/// lifetime because it owns the table's MMIO mapping. The vectors' owned IRQ
/// bindings live in the device's resource bag (not here), so this stays
/// `Clone`.
#[derive(Clone)]
pub struct VirtioMsixState {
    /// Parsed MSI-X capability from PCI config space.
    pub cap: crate::msix::MsixCapability,
    /// Mapped MSI-X table (MMIO).
    pub table: crate::msix::MsixTable,
    /// Allocated IDT vector for each queue (0 = not assigned).
    pub queue_vectors: [u8; MAX_MSIX_QUEUES],
    /// Number of queues that were assigned MSI-X vectors.
    pub num_queues: u8,
}

impl VirtioMsixState {
    /// Get the MSI-X table entry index for `queue_idx`.
    ///
    /// Convention: entry 0..N-1 map to queues 0..N-1 (no config-change entry).
    /// Returns [`VIRTIO_MSI_NO_VECTOR`] if the queue has no vector assigned.
    #[inline]
    pub fn queue_msix_entry(&self, queue_idx: u16) -> u16 {
        let i = queue_idx as usize;
        if i < self.num_queues as usize && self.queue_vectors[i] != 0 {
            queue_idx
        } else {
            VIRTIO_MSI_NO_VECTOR
        }
    }

    /// IDT vector allocated to the given queue, or `None`.
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

// =============================================================================
// Device Status Helpers
// =============================================================================

/// Set the device status register
#[inline]
pub fn set_device_status(cfg: &MmioRegion, status: u8) {
    cfg.write::<u8>(COMMON_CFG_DEVICE_STATUS, status);
}

/// Get the device status register
#[inline]
pub fn get_device_status(cfg: &MmioRegion) -> u8 {
    cfg.read::<u8>(COMMON_CFG_DEVICE_STATUS)
}

/// Reset the device (set status to 0)
#[inline]
pub fn reset_device(cfg: &MmioRegion) {
    set_device_status(cfg, 0);
}

// =============================================================================
// VirtIO Memory Barrier Abstractions
// =============================================================================

/// VirtIO write memory barrier.
///
/// Per VirtIO spec 2.7.7: "A write memory barrier before updating avail idx"
/// Ensures descriptor writes are visible before publishing availability.
#[inline(always)]
pub fn virtio_wmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}

/// VirtIO read memory barrier.
///
/// Per VirtIO spec 2.7.13: "A read memory barrier before reading used buffers"
/// Ensures used_idx observation happens-before reading completion data.
#[inline(always)]
pub fn virtio_rmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
}
