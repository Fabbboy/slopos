//! `BoundPlatformDevice` — the capability handed to a platform (ACPI) driver's
//! `probe`, the non-PCI sibling of [`crate::driver_core::bound::BoundDevice`].
//!
//! A platform device's resources are I/O ports and a legacy (IOAPIC-routed) IRQ
//! line declared in its ACPI `_CRS`, rather than PCI BARs/MSI. Each vend
//! acquires through an ostd primitive and hands ownership to the device's
//! [`Devres`] bag, so a probe that fails partway releases everything it touched
//! (in reverse order) when the registry drops the bag. Pure-safe glue: all
//! `unsafe` lives in the ostd primitives it drives.

use slopos_ostd::dev::Devres;
use slopos_ostd::io::port::{IoPort, IoPortRegistry};
use slopos_ostd::irq::{IRQ_BASE_VECTOR, IrqAllocator, IrqContext};

use crate::driver_core::bound::BoundError;
use crate::platform_bus::PlatformDeviceInfo;

/// The capability a platform driver's probe drives to acquire its resources.
///
/// Borrows the device's enumeration record and its [`Devres`] bag for the
/// duration of the probe.
pub struct BoundPlatformDevice<'d> {
    info: &'d PlatformDeviceInfo,
    res: &'d mut Devres,
}

impl<'d> BoundPlatformDevice<'d> {
    /// Pair a device record with the bag its resources attach to.
    pub fn new(info: &'d PlatformDeviceInfo, res: &'d mut Devres) -> Self {
        Self { info, res }
    }

    /// The device's ACPI enumeration record (matched id, `_CRS` I/O windows +
    /// IRQ, presence). `PlatformDeviceInfo` is `Copy`, so a probe typically
    /// snapshots it before vending, freeing the borrow.
    #[inline]
    pub fn info(&self) -> &PlatformDeviceInfo {
        self.info
    }

    /// Reserve a single I/O port, certified insensitive by the platform, and
    /// hand the handle to the bag so it releases on probe failure / unbind.
    /// Returns a `Copy` of the handle (the bag cell is the ownership anchor).
    pub fn reserve_io_port(&mut self, port: u16) -> Result<IoPort<u8>, BoundError> {
        let handle = IoPortRegistry::reserve::<u8>(port).map_err(BoundError::IoPort)?;
        self.res
            .attach(handle)
            .map_err(|_| BoundError::OutOfMemory)?;
        Ok(handle)
    }

    /// Reserve the IDT vector for a hardware-pinned legacy IRQ `line`, install
    /// `handler`, program the IOAPIC route, unmask it, and hand the owned IRQ
    /// binding to the bag (so it is masked + released on probe failure/unbind).
    /// Returns the absolute IDT vector.
    pub fn request_legacy_irq<F>(&mut self, line: u8, handler: F) -> Result<u8, BoundError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        let vector = IRQ_BASE_VECTOR.wrapping_add(line);
        let irq_line = IrqAllocator::reserve_specific(vector).map_err(BoundError::Irq)?;
        let owned = irq_line
            .register_callback_owned(handler)
            .map_err(BoundError::Irq)?;
        self.res
            .attach(owned)
            .map_err(|_| BoundError::OutOfMemory)?;

        // Configure the IOAPIC RTE for this legacy line and unmask it. Mirrors
        // the boot `irq::init` path; the route is programmed here (not in
        // `setup_ioapic_routes`) for devices the platform bus owns.
        crate::irq::program_ioapic_route(line);
        slopos_kernel_services::driver_runtime::irq_enable_line(line);
        Ok(vector)
    }
}
