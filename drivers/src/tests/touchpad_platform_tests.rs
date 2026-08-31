//! Regression guard for the I²C-HID touchpad's platform-bus registration.
//!
//! The bring-up itself needs the real controller, so it is not unit-tested.
//! What is tested is the wiring that a QEMU boot cannot exercise at all,
//! because no `PNP0C50` node exists there: the driver must ask for the
//! I²C-connector walker, and the probe must read its device from the
//! enumeration record.

use slopos_ostd::dev::Devres;
use slopos_testing::{TestResult, fail, pass};

use crate::platform_bus::{
    BoundPlatformDevice, MAX_PLATFORM_IO, PlatformDeviceInfo, PlatformDriverEntry,
    PlatformIoWindow, PlatformMatch, ProbeOutcome, driver_registry_iter,
};

fn touchpad_entry() -> Option<&'static PlatformDriverEntry> {
    driver_registry_iter().find(|e| e.name == "i2c-hid-touchpad")
}

/// A device matched by `_HID`/`_CID` alone carries no I²C connector, so the
/// touchpad must declare `I2cHid` to be handed one. Declaring `HidCid` would
/// still match the node on real hardware and then find `info().i2c == None` —
/// which is exactly the bug this guards.
pub fn test_touchpad_requests_the_i2c_walker() -> TestResult {
    let entry = match touchpad_entry() {
        Some(e) => e,
        None => return fail!("i2c-hid-touchpad not found in the platform driver registry"),
    };
    let mut saw_i2c_hid = false;
    for m in entry.match_table {
        match m {
            PlatformMatch::I2cHid(id) => {
                if *id != b"PNP0C50" {
                    return fail!("i2c-hid-touchpad must match PNP0C50, got {:?}", id);
                }
                saw_i2c_hid = true;
            }
            PlatformMatch::HidCid(id) => {
                return fail!(
                    "i2c-hid-touchpad must not match {:?} via the _CRS walker: it would \
                     bind a device with no I2C connector",
                    id
                );
            }
        }
    }
    if !saw_i2c_hid {
        return fail!("i2c-hid-touchpad declares no I2cHid match rule");
    }
    // The device is found by ACPI on real hardware, so the not-found fallback
    // seam must not be what surfaces it.
    if entry.fallback.is_some() {
        return fail!("i2c-hid-touchpad must not rely on the device-not-found fallback");
    }
    pass!()
}

/// A record with no connector must decline rather than bind. This is the shape
/// the driver saw on real hardware while the QEMU log was identical to a
/// healthy run.
pub fn test_touchpad_declines_without_connector() -> TestResult {
    let entry = match touchpad_entry() {
        Some(e) => e,
        None => return fail!("i2c-hid-touchpad not registered"),
    };
    let info = PlatformDeviceInfo {
        matched_id: b"PNP0C50",
        io: [PlatformIoWindow::default(); MAX_PLATFORM_IO],
        io_count: 0,
        irq: None,
        present: None,
        i2c: None,
    };
    let mut devres = Devres::new();
    let mut bound = BoundPlatformDevice::new(&info, &mut devres);
    match (entry.probe)(&mut bound) {
        Ok(ProbeOutcome::Declined) => pass!(),
        Ok(ProbeOutcome::Bound) => fail!("bound a device with no I2C connector"),
        Err(e) => fail!("expected Declined for a connectorless record, got {:?}", e),
    }
}

slopos_testing::stest!(
    name = test_touchpad_requests_the_i2c_walker,
    suite = touchpad_platform
);
slopos_testing::stest!(
    name = test_touchpad_declines_without_connector,
    suite = touchpad_platform
);
