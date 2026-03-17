//! VirtIO common infrastructure
//!
//! This module provides shared types, constants, and utilities for VirtIO device drivers.
//! It eliminates code duplication between virtio-blk and future virtio drivers.

pub mod pci;
pub mod queue;

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_mm::mmio::MmioRegion;

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
#[derive(Clone, Copy, Default)]
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

/// Per-queue interrupt completion event.
///
/// The MSI-X handler calls [`signal`] when the device places entries
/// in the used ring. The I/O path calls [`wait_timeout_ms`] which
/// blocks on a scheduler-backed wait queue until the IRQ wakes it or
/// the timeout elapses.
pub struct QueueEvent {
    signaled: AtomicBool,
}

impl QueueEvent {
    pub const fn new() -> Self {
        Self {
            signaled: AtomicBool::new(false),
        }
    }

    /// Called from the MSI-X IRQ handler to signal I/O completion.
    #[inline]
    pub fn signal(&self) {
        self.signaled.store(true, Ordering::Release);
    }

    /// Reset the event (consume the signal).
    #[inline]
    pub fn reset(&self) {
        self.signaled.store(false, Ordering::Release);
    }

    /// Try to consume the signal. Returns `true` if it was set.
    #[inline]
    pub fn try_consume(&self) -> bool {
        self.signaled
            .compare_exchange(true, false, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Wait for signal or timeout.
    ///
    /// Uses x86 `cli; sti; hlt` to atomically enable interrupts and
    /// halt, preventing the lost-wakeup race. The HPET main counter
    /// provides real-time deadline tracking.
    ///
    /// Returns `true` if signaled, `false` on timeout.
    pub fn wait_timeout_ms(&self, timeout_ms: u32) -> bool {
        use crate::hpet;

        if self.try_consume() {
            return true;
        }

        let period_fs = hpet::period_fs();
        if period_fs == 0 {
            for _ in 0..100_000u32 {
                if self.try_consume() {
                    return true;
                }
                core::hint::spin_loop();
            }
            return self.try_consume();
        }

        let ticks_needed =
            ((timeout_ms as u128) * 1_000_000_000_000u128 / period_fs as u128) as u64;
        let start = hpet::read_counter();
        let allow_hlt_wait = slopos_lib::get_current_cpu() == 0;

        loop {
            unsafe {
                core::arch::asm!("cli", options(nomem, nostack));
            }

            if self.signaled.load(Ordering::Acquire) {
                unsafe {
                    core::arch::asm!("sti", options(nomem, nostack));
                }
                self.signaled.store(false, Ordering::Release);
                return true;
            }

            if hpet::read_counter().wrapping_sub(start) >= ticks_needed {
                unsafe {
                    core::arch::asm!("sti", options(nomem, nostack));
                }
                return false;
            }

            if allow_hlt_wait {
                // SAFETY: `sti; hlt` atomically enables interrupts then halts.
                unsafe {
                    core::arch::asm!("sti", "hlt", options(nomem, nostack));
                }
            } else {
                // AP CPUs do not run a local periodic timer in current SlopOS
                // bring-up; avoid hlt there or they may never wake.
                unsafe {
                    core::arch::asm!("sti", options(nomem, nostack));
                }
                core::hint::spin_loop();
            }
        }
    }
}

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

/// Per-device MSI-X state produced by [`pci::try_setup_msix`].
///
/// Stores the mapped MSI-X table, the allocated IDT vectors for each
/// virtqueue, and the overall enable state.  Callers must keep this alive
/// for the lifetime of the device because it owns the MMIO mappings and
/// the vector allocations.
#[derive(Clone, Copy)]
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
