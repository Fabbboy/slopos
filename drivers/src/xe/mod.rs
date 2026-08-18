//! Hardware-sequencing half of the Intel xe display driver.
//!
//! Owns the device-facing work the pure [`crate::xe_logic`] layer deliberately
//! avoids: PCI binding, BAR0 mapping, reading live display registers, and
//! inheriting the firmware modeset to re-point the active plane at our own
//! linear framebuffer. This module maps the register window once and hands a
//! shared handle to [`repoint::run`]; all claim/commit and snapshot/rollback
//! logic lives there, not in this dispatch.

mod cursor;
mod ddb;
mod diag;
mod fb_mem;
mod ggtt;
mod mmio_map;
mod pipe;
mod plane;
mod present;
mod repoint;
mod snapshot;
mod watchdog;

use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::driver_core::bound::BoundDevice;
use crate::pci::{PciMatch, PciProbeError, ProbeOutcome};
use crate::pci_defs::PCI_CLASS_DISPLAY;
use crate::xe_logic::cmdline::XeConfig;
use crate::xe_logic::platform::{self, PCI_VENDOR_INTEL};

/// Command-line configuration parsed by boot before probe runs. `None` until
/// [`set_config`] installs the parsed knobs; readers fall back to the defaults.
static XE_CONFIG: SpinLock<Option<XeConfig>> =
    SpinLock::new(None, lock_class!("XE_CONFIG", LOCK_LEVEL_RESOURCE));

/// Install the boot-parsed `xe.*` configuration. Called once from the PCI-init
/// boot step before driver probe so [`xe_probe`] can read it back.
pub fn set_config(cfg: XeConfig) {
    *XE_CONFIG.lock() = Some(cfg);
}

fn config() -> XeConfig {
    let guard = XE_CONFIG.lock();
    guard.unwrap_or_default()
}

/// PCI probe entry: identify the platform, then drive the display.
///
/// On a recognised device the driver inherits the firmware modeset and takes
/// over scanout via [`repoint::run`] — that is the default, no knob required.
/// `xe.modeset=off` is the `nomodeset` escape: the driver maps the register
/// window, optionally dumps diagnostics, and stays on the firmware framebuffer
/// without writing a single display register.
fn xe_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    let cfg = config();

    let did = cfg.force_did.unwrap_or(info.device_id);
    let Some(platform) = platform::identify(info.vendor_id, did) else {
        klog_info!("XE: unrecognised display device 0x{:04x}; declining", did);
        return Ok(ProbeOutcome::Declined);
    };

    klog_info!("XE: matched {} (did 0x{:04x})", platform.name, did);

    if !cfg.modeset && !cfg.diag {
        klog_info!("XE: xe.modeset=off; firmware framebuffer retained");
        return Ok(ProbeOutcome::Declined);
    }

    let mmio = match mmio_map::map_gttmmadr(bound) {
        Ok(mmio) => mmio,
        Err(_) => {
            klog_info!("XE: GTTMMADR map failed; cannot drive display, declining");
            return Ok(ProbeOutcome::Declined);
        }
    };

    if cfg.diag {
        diag::dump(&mmio, &info, platform);
    }

    // `nomodeset` escape: diagnostics were emitted (if asked), but we write no
    // display register and leave the firmware scanout driving the panel.
    if !cfg.modeset {
        klog_info!("XE: xe.modeset=off; diagnostics only, firmware framebuffer retained");
        return Ok(ProbeOutcome::Declined);
    }

    // Drive the display: inherit the firmware modeset and take over scanout. The
    // claim/commit, snapshot/rollback, cursor, and present logic all live in
    // `repoint::run`, which restores the firmware framebuffer on any failure.
    repoint::run(&mmio, cfg, platform)
}

crate::pci_driver! {
    pub static XE_DRIVER = {
        name: "intel-xe",
        match_table: &[PciMatch::VendorClass {
            vendor: PCI_VENDOR_INTEL,
            class: PCI_CLASS_DISPLAY,
        }],
        probe: xe_probe,
    };
}
