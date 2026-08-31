//! The I²C-HID touchpad as a platform (ACPI) device driver.
//!
//! The touchpad's ACPI record is not a `_CRS` of I/O ports and an IRQ line —
//! it is an `I2cSerialBus` connector plus a `GpioInt`, behind a `_DSM` the
//! platform bus's small-descriptor walker does not evaluate. So the device is
//! surfaced through the registry's `fallback` seam, which exists for exactly
//! this: a driver that knows how to find its own device supplies the record.
//!
//! Configuration comes from a boot step rather than the probe, because a probe
//! cannot reach the framebuffer geometry or the kernel cmdline.

use slopos_acpi::aml::{self, AcpiI2cHid, HhdmHost};
use slopos_acpi::tables::AcpiTables;
use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::platform_bus::{
    BoundPlatformDevice, MAX_PLATFORM_IO, PlatformDeviceInfo, PlatformIoWindow, PlatformMatch,
    PlatformProbeError, ProbeOutcome,
};

use super::{TouchpadError, bring_up};

/// The generic I²C-HID `_CID`. Matching is by this id alone; the device record
/// itself is built by [`i2c_hid_fallback`].
const HID_I2C_ID: &[u8] = b"PNP0C50";

/// Boot-parsed configuration, installed before the platform bus probes.
#[derive(Clone, Copy)]
pub struct TouchpadConfig {
    pub rsdp_phys: u64,
    pub width: u32,
    pub height: u32,
    pub debug: bool,
    pub force_poll: bool,
}

impl TouchpadConfig {
    const fn empty() -> Self {
        Self {
            rsdp_phys: 0,
            width: 0,
            height: 0,
            debug: false,
            force_poll: false,
        }
    }
}

/// Boot configuration plus what the namespace walk found, under one lock: both
/// are written once before probe and read only by it.
struct PlatformState {
    cfg: TouchpadConfig,
    discovered: Option<AcpiI2cHid>,
}

static STATE: SpinLock<PlatformState> = SpinLock::new(
    PlatformState {
        cfg: TouchpadConfig::empty(),
        discovered: None,
    },
    lock_class!("TOUCHPAD_PLATFORM", LOCK_LEVEL_RESOURCE),
);

/// Install the boot-parsed configuration. Called from the PCI/platform boot
/// step before [`probe`] runs; without it the driver has no RSDP and declines.
pub fn set_config(cfg: TouchpadConfig) {
    STATE.lock().cfg = cfg;
}

fn config() -> TouchpadConfig {
    STATE.lock().cfg
}

crate::platform_driver! {
    pub static I2C_HID_TOUCHPAD = {
        name: "i2c-hid-touchpad",
        match_table: &[PlatformMatch::HidCid(HID_I2C_ID)],
        fallback: Some(i2c_hid_fallback),
        probe: probe,
    };
}

/// Locate the I²C-HID device in the ACPI namespace. The `_DSM`-evaluating
/// walker is the only thing that resolves it, and it is not something the
/// generic `_CRS` enumerator can do.
///
/// The argument is the FADT's 8042 bit, which is irrelevant here.
fn i2c_hid_fallback(_has_8042: bool) -> Option<PlatformDeviceInfo> {
    let cfg = config();
    if cfg.rsdp_phys == 0 || crate::i2c::lpss_disabled() {
        return None;
    }
    let tables = AcpiTables::from_phys(cfg.rsdp_phys)?;
    let found = aml::scan_i2c_hid(&tables, &HhdmHost, cfg.debug)?;
    STATE.lock().discovered = Some(found);
    Some(PlatformDeviceInfo {
        matched_id: HID_I2C_ID,
        io: [PlatformIoWindow::default(); MAX_PLATFORM_IO],
        io_count: 0,
        irq: None,
        // The GpioInt is wired through pinctrl, not the IOAPIC line the bus
        // would route; `_STA` is not consulted for a `_DSM`-discovered device.
        present: None,
    })
}

fn probe(_bound: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    if crate::i2c::lpss_disabled() {
        return Ok(ProbeOutcome::Declined);
    }
    // One acquire: the probe must not hold the lock across bring-up, which does
    // millisecond-scale I²C transfers.
    let (cfg, discovered) = {
        let st = STATE.lock();
        (st.cfg, st.discovered)
    };
    let Some(found) = discovered else {
        klog_info!("touchpad: no I2C-HID device found in ACPI namespace");
        return Ok(ProbeOutcome::Declined);
    };

    match bring_up(&found, cfg.width, cfg.height, cfg.debug, cfg.force_poll) {
        Ok(()) => Ok(ProbeOutcome::Bound),
        // The parent I²C controller is bound by a PCI driver, and PCI probe has
        // already run to completion by the time any platform device is offered.
        // A retry cannot change that answer, so this declines rather than
        // deferring.
        Err(TouchpadError::NoParentBus) => Ok(ProbeOutcome::Declined),
        Err(TouchpadError::NoDigitizer) => Err(PlatformProbeError::Mismatch),
        Err(TouchpadError::BringUp) => Err(PlatformProbeError::DeviceFault),
    }
}
