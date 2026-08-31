//! The I²C-HID touchpad as a platform (ACPI) device driver.
//!
//! Matching is by `_CID`/`_HID` like any platform device, but the resources
//! come from the `_DSM`-evaluating walker: an I²C-attached device declares an
//! `I2cSerialBus` connector and a `GpioInt` rather than the `_CRS` I/O window
//! and IOAPIC line the small-descriptor walker reads.
//!
//! Configuration comes from a boot step rather than the probe, because a probe
//! cannot reach the framebuffer geometry or the kernel cmdline.

use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::platform_bus::{BoundPlatformDevice, PlatformMatch, PlatformProbeError, ProbeOutcome};

use super::{TouchpadError, bring_up};

/// The generic I²C-HID `_CID`.
const HID_I2C_ID: &[u8] = b"PNP0C50";

/// Boot-parsed configuration, installed before the platform bus probes.
#[derive(Clone, Copy)]
pub struct TouchpadConfig {
    pub width: u32,
    pub height: u32,
    pub debug: bool,
    pub force_poll: bool,
}

impl TouchpadConfig {
    const fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            debug: false,
            force_poll: false,
        }
    }
}

static CONFIG: SpinLock<TouchpadConfig> = SpinLock::new(
    TouchpadConfig::empty(),
    lock_class!("TOUCHPAD_PLATFORM", LOCK_LEVEL_RESOURCE),
);

/// Install the boot-parsed configuration, before the platform bus probes.
pub fn set_config(cfg: TouchpadConfig) {
    *CONFIG.lock() = cfg;
}

crate::platform_driver! {
    pub static I2C_HID_TOUCHPAD = {
        name: "i2c-hid-touchpad",
        match_table: &[PlatformMatch::I2cHid(HID_I2C_ID)],
        probe: probe,
    };
}

fn probe(bound: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    if crate::i2c::lpss_disabled() {
        return Ok(ProbeOutcome::Declined);
    }
    let Some(found) = bound.info().i2c else {
        klog_info!("touchpad: matched PNP0C50 but no I2C connector in its resources");
        return Ok(ProbeOutcome::Declined);
    };
    let cfg = *CONFIG.lock();

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
