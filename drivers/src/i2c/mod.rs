//! I²C bus subsystem.
//!
//! Provides the Synopsys DesignWare / Intel LPSS master controller
//! ([`designware`]) that carries the I²C-HID touchpad on modern Intel
//! laptops, the PCI-probe glue that binds it ([`pci`]), and a small
//! registry mapping a claimed controller back to its PCI location so the
//! ACPI-discovered touchpad can find the bus it lives on.

pub mod designware;
pub mod pci;

pub use designware::{DesignWareI2c, I2cError, I2cSegment, Mmio32};
use slopos_ostd::lock_class;

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::KArc;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

/// Gate for the LPSS I²C probe + touchpad bring-up. Disabled with the
/// `tp.off` cmdline flag.
static LPSS_DISABLED: AtomicBool = AtomicBool::new(false);

pub fn set_lpss_disabled(disabled: bool) {
    LPSS_DISABLED.store(disabled, Ordering::Release);
}

pub fn lpss_disabled() -> bool {
    LPSS_DISABLED.load(Ordering::Acquire)
}

/// A claimed, initialised I²C controller plus the PCI Bus/Device/Function
/// it was found at (so an ACPI `_ADR` can be resolved back to it).
///
/// The transfer methods take `&self`; the underlying [`DesignWareI2c`]
/// drives the hardware through interior-mutable [`MmioRegion`] access.
/// There is intentionally no transaction lock here: the touchpad poll
/// thread is the sole consumer of its bus. A second consumer on the same
/// bus must add serialization (a sleeping `Mutex`, never an IRQ-off
/// spinlock — transfers can run for hundreds of microseconds).
pub struct I2cBus {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    ctrl: DesignWareI2c<MmioRegion>,
}

impl I2cBus {
    pub fn new(bus: u8, device: u8, function: u8, ctrl: DesignWareI2c<MmioRegion>) -> Self {
        Self {
            bus,
            device,
            function,
            ctrl,
        }
    }

    /// Write `tx`, repeated-START, read into `rx` — the register-addressed
    /// I²C-HID read shape.
    #[inline]
    pub fn write_read(&self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), I2cError> {
        self.ctrl.write_read(addr, tx, rx)
    }

    /// Write `tx` then STOP.
    #[inline]
    pub fn write(&self, addr: u8, tx: &[u8]) -> Result<(), I2cError> {
        self.ctrl.write(addr, tx)
    }

    /// Bare read into `rx` (no preceding register address) — the I²C-HID
    /// input-report read.
    #[inline]
    pub fn read(&self, addr: u8, rx: &mut [u8]) -> Result<(), I2cError> {
        self.ctrl.read(addr, rx)
    }
}

const MAX_I2C_BUSES: usize = 8;

static I2C_BUSES: SpinLock<[Option<KArc<I2cBus>>; MAX_I2C_BUSES]> = SpinLock::new(
    [const { None }; MAX_I2C_BUSES],
    lock_class!("I2C_BUSES", LOCK_LEVEL_REGISTRY),
);

/// Record a claimed controller in the registry. Called from the PCI probe.
pub fn register_bus(bus: KArc<I2cBus>) {
    let mut table = I2C_BUSES.lock();
    for slot in table.iter_mut() {
        if slot.is_none() {
            *slot = Some(bus);
            return;
        }
    }
}

/// Look up a claimed controller by its PCI Bus/Device/Function.
pub fn bus_by_bdf(bus: u8, device: u8, function: u8) -> Option<KArc<I2cBus>> {
    let table = I2C_BUSES.lock();
    for slot in table.iter() {
        if let Some(b) = slot {
            if b.bus == bus && b.device == device && b.function == function {
                return Some(b.clone());
            }
        }
    }
    None
}

/// Resolve an ACPI `_ADR` value (encoding `device << 16 | function`) on
/// PCI bus 0 to a claimed controller. This is how the touchpad's parent
/// I²C device (e.g. `\_SB.PC00.I2C1`, `_ADR = 0x00150001`) maps to the
/// controller the PCI probe claimed.
pub fn bus_by_acpi_adr(adr: u32) -> Option<KArc<I2cBus>> {
    let device = ((adr >> 16) & 0xff) as u8;
    let function = (adr & 0xff) as u8;
    bus_by_bdf(0, device, function)
}
