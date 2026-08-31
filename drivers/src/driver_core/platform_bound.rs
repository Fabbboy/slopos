//! The platform (ACPI) bus's [`BoundDevice`] vends.
//!
//! A platform device's own resources are the I/O ports and legacy
//! (IOAPIC-routed) IRQ line its ACPI `_CRS` declares; the MMIO/DMA/IRQ vends it
//! shares with every other bus live in [`crate::driver_core::bound`].

use slopos_ostd::io::port::{IoPort, IoPortRegistry};
use slopos_ostd::irq::{IRQ_BASE_VECTOR, IrqAllocator, IrqContext};

use crate::driver_core::bound::BoundError;
use crate::driver_core::bus::BoundDevice;
use crate::platform_bus::PlatformBus;

impl<'d> BoundDevice<'d, PlatformBus> {
    /// Reserve a single I/O port. The returned handle is a `Copy`; the bag cell
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
