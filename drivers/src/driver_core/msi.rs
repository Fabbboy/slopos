//! Shared MSI-X / MSI setup orchestration.
//!
//! A thin layer over the per-vector primitives in [`crate::msix`] /
//! [`crate::msi`] / `crate::msi_common`, not a third MSI abstraction. Its one
//! new behaviour is ownership: each allocated vector + callback becomes an
//! [`OwnedIrq`] in the device's [`Devres`] bag, so it is released on probe
//! failure or unbind.

use slopos_ostd::irq::{IrqAllocator, OwnedIrq};
use slopos_ostd::{KVec, klog_debug};

use crate::driver_core::bound::BoundDevice;
use crate::msi::{self, MsiCapability};
use crate::msix::{self, MsixCapability, MsixTable};

pub enum IrqMechanism {
    /// Per-queue vectors via the MSI-X table.
    Msix {
        cap: MsixCapability,
        /// Callers keep it alive for the device lifetime.
        table: MsixTable,
    },
    /// A single shared vector via the MSI capability.
    Msi {
        cap: MsiCapability,
        /// Allocated IDT vector.
        vector: u8,
    },
}

/// The target LAPIC for device interrupts (the BSP).
const TARGET_APIC_ID: u8 = 0;

/// Set up MSI-X: one IDT vector per queue, each programmed into the device's
/// MSI-X table and bound to a per-queue dispatch closure.
///
/// `handler` is called with the queue index when its vector fires.
/// `vectors_out` must be at least `num_queues` long; `vectors_out[i]` receives
/// the IDT vector allocated to queue `i`.
///
/// On any failure returns `None` having released every vector it allocated this
/// call, so a caller can cleanly fall back to MSI. On success every binding
/// moves into the device's [`Devres`] bag.
pub fn setup_msix<F>(
    bound: &mut BoundDevice<'_>,
    num_queues: usize,
    vectors_out: &mut [u8],
    handler: F,
) -> Option<(MsixCapability, MsixTable)>
where
    F: Fn(u8) + Clone + Send + Sync + 'static,
{
    let info = *bound.info();
    let cap_offset = info.msix_cap_offset?;
    if num_queues == 0 || num_queues > vectors_out.len() {
        return None;
    }

    let cap = msix::msix_read_capability(info.bus, info.device, info.function, cap_offset);
    if (cap.table_size as usize) < num_queues {
        klog_debug!(
            "msi: {}:{}.{} has {} MSI-X entries, need {}",
            info.bus,
            info.device,
            info.function,
            cap.table_size,
            num_queues,
        );
        return None;
    }

    let table = match msix::msix_map_table(&info, &cap) {
        Ok(t) => t,
        Err(e) => {
            klog_debug!("msi: MSI-X table map failed: {:?}", e);
            return None;
        }
    };

    let mut owned: KVec<OwnedIrq> = KVec::new();
    for i in 0..num_queues {
        let line = match IrqAllocator::alloc() {
            Ok(l) => l,
            Err(_) => {
                klog_debug!("msi: vector pool exhausted at queue {}", i);
                return None;
            }
        };
        let vector = line.vector();

        // Program the table entry before installing the callback so a configure
        // failure drops the bare line (no dispatch slot was populated).
        if let Err(e) = msix::msix_configure(&table, i as u16, vector, TARGET_APIC_ID) {
            klog_debug!("msi: configure MSI-X entry {} failed: {:?}", i, e);
            return None;
        }

        let h = handler.clone();
        let queue_idx = i as u8;
        let owned_irq = match line.register_callback_owned(move |_ctx| h(queue_idx)) {
            Ok(o) => o,
            Err(_) => {
                klog_debug!("msi: register callback failed at queue {}", i);
                return None;
            }
        };
        if owned.push(owned_irq).is_err() {
            return None;
        }
        vectors_out[i] = vector;
    }

    for owned_irq in owned.drain(..) {
        if bound.attach(owned_irq).is_err() {
            return None;
        }
    }
    Some((cap, table))
}

/// Set up MSI: one shared IDT vector for the whole device, bound to a dispatch
/// closure invoked with queue index `0`. The binding moves into the device's
/// [`Devres`] bag. Returns the capability + vector, or `None` if the device has
/// no MSI capability or allocation/configuration fails.
pub fn setup_msi<F>(bound: &mut BoundDevice<'_>, handler: F) -> Option<(MsiCapability, u8)>
where
    F: Fn(u8) + Send + Sync + 'static,
{
    let info = *bound.info();
    let cap_offset = info.msi_cap_offset?;
    let cap = msi::msi_read_capability(info.bus, info.device, info.function, cap_offset);

    let line = IrqAllocator::alloc().ok()?;
    let vector = line.vector();
    if msi::msi_configure(
        info.bus,
        info.device,
        info.function,
        &cap,
        vector,
        TARGET_APIC_ID,
    )
    .is_err()
    {
        return None;
    }

    let owned_irq = line.register_callback_owned(move |_ctx| handler(0)).ok()?;
    bound.attach(owned_irq).ok()?;
    Some((cap, vector))
}

/// Set up the best available interrupt mechanism: MSI-X (per-queue vectors)
/// first, then MSI (single shared vector). Returns `None` if the device has
/// neither.
pub fn setup_interrupts<F>(
    bound: &mut BoundDevice<'_>,
    num_queues: usize,
    vectors_out: &mut [u8],
    handler: F,
) -> Option<IrqMechanism>
where
    F: Fn(u8) + Clone + Send + Sync + 'static,
{
    if let Some((cap, table)) = setup_msix(bound, num_queues, vectors_out, handler.clone()) {
        return Some(IrqMechanism::Msix { cap, table });
    }
    if let Some((cap, vector)) = setup_msi(bound, handler) {
        return Some(IrqMechanism::Msi { cap, vector });
    }
    None
}
