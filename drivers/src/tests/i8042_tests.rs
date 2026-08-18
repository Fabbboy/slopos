//! Regression guard for the i8042 platform-bus driver registration.
//!
//! Asserts on the *live* `.platform_driver_registry`, unlike `platform_binding.rs`
//! which drives the matchmaker over synthetic drivers. The probe itself does live
//! controller I/O and is exercised at boot, so it is not unit-tested here.

use slopos_testing::{TestResult, fail, pass};

use crate::platform_bus::{PlatformDriverEntry, driver_registry_iter};

/// The live registry entry for the i8042 keyboard driver, if registered.
fn i8042_entry() -> Option<&'static PlatformDriverEntry> {
    driver_registry_iter().find(|e| e.name == "i8042-kbd")
}

pub fn test_i8042_driver_registered() -> TestResult {
    let entry = match i8042_entry() {
        Some(e) => e,
        None => return fail!("i8042-kbd not found in the platform driver registry"),
    };
    if !entry.match_table.iter().any(|m| m.id() == b"PNP0303") {
        return fail!("i8042-kbd must match PNP0303");
    }
    if entry.fallback.is_none() {
        return fail!("i8042-kbd must carry an architectural fallback");
    }
    pass!()
}

pub fn test_i8042_fallback_synthesizes_when_8042_present() -> TestResult {
    let entry = match i8042_entry() {
        Some(e) => e,
        None => return fail!("i8042-kbd not registered"),
    };
    let fb = match entry.fallback {
        Some(fb) => fb,
        None => return fail!("i8042-kbd has no fallback"),
    };

    // FADT advertises an 8042 → synthesize the architectural keyboard.
    let dev = match fb(true) {
        Some(d) => d,
        None => return fail!("fallback(true) must synthesize a device"),
    };
    if dev.matched_id != b"PNP0303" {
        return fail!("fallback device id = {:?}", dev.matched_id);
    }
    let io = dev.io_ports();
    if io.len() != 2 || io[0].base != 0x60 || io[1].base != 0x64 {
        return fail!("fallback IO windows = {:?}", io);
    }
    match dev.irq {
        Some(q) if q.line == 1 => {}
        other => return fail!("fallback IRQ = {:?}", other),
    }
    // Presence unknown so the probe's gate proceeds rather than declining.
    if dev.present.is_some() {
        return fail!("fallback present must be None, got {:?}", dev.present);
    }

    // No 8042 advertised → no synthesized device.
    if fb(false).is_some() {
        return fail!("fallback(false) must not synthesize a device");
    }
    pass!()
}

slopos_testing::stest!(name = test_i8042_driver_registered, suite = i8042);
slopos_testing::stest!(
    name = test_i8042_fallback_synthesizes_when_8042_present,
    suite = i8042
);
