//! `BoundPlatformDevice` — the capability handed to a platform (ACPI) driver's
//! `probe`, the non-PCI sibling of [`crate::driver_core::bound::BoundDevice`].
//!
//! A platform device's resources are the I/O ports and legacy (IOAPIC-routed)
//! IRQ line its ACPI `_CRS` declares. Every vend hands ownership to the device's
//! [`Devres`] bag, so a probe that fails partway releases what it touched, in
//! reverse order, when the registry drops the bag.

use slopos_ostd::dev::Devres;
use slopos_ostd::io::port::{IoPort, IoPortRegistry};
use slopos_ostd::irq::{IRQ_BASE_VECTOR, IrqAllocator, IrqContext};

use crate::driver_core::bound::BoundError;
use crate::platform_bus::PlatformDeviceInfo;

/// The capability a platform driver's probe drives to acquire its resources.
pub struct BoundPlatformDevice<'d> {
    info: &'d PlatformDeviceInfo,
    res: &'d mut Devres,
}

impl<'d> BoundPlatformDevice<'d> {
    /// Pair a device record with the bag its resources attach to.
    pub fn new(info: &'d PlatformDeviceInfo, res: &'d mut Devres) -> Self {
        Self { info, res }
    }

    /// The device's ACPI enumeration record. `PlatformDeviceInfo` is `Copy`, so
    /// a probe can snapshot it before vending and free the borrow.
    #[inline]
    pub fn info(&self) -> &PlatformDeviceInfo {
        self.info
    }

    /// Reserve a single I/O port and hand the handle to the bag, so it releases
    /// on probe failure / unbind. The returned handle is a `Copy`; the bag cell
    /// is the ownership anchor.
    pub fn reserve_io_port(&mut self, port: u16) -> Result<IoPort<u8>, BoundError> {
        let handle = IoPortRegistry::reserve::<u8>(port).map_err(BoundError::IoPort)?;
        self.res
            .attach(handle)
            .map_err(|_| BoundError::OutOfMemory)?;
        Ok(handle)
    }

    /// Reserve the IDT vector for a hardware-pinned legacy IRQ `line`, install
    /// `handler`, program the IOAPIC route, unmask it, and hand the owned IRQ
    /// binding to the bag. Returns the absolute IDT vector.
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

        // Routed here rather than in `setup_ioapic_routes`, which does not know
        // about lines the platform bus owns.
        crate::irq::program_ioapic_route(line);
        slopos_kernel_services::driver_runtime::irq_enable_line(line);
        Ok(vector)
    }
}
