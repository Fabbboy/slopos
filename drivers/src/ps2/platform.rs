//! The i8042 PS/2 controller as a **platform (ACPI) device-driver**.
//!
//! ACPI describes the PC keyboard as a `PNP0303` device under the LPC bridge
//! with fixed I/O ports (0x60 data, 0x64 status/command) and a legacy IRQ in its
//! `_CRS`. This module binds it through the platform bus ([`crate::platform_bus`])
//! exactly like a PCI driver: a link-section [`platform_driver!`] registration, a
//! `probe` that resource-claims its ports + IRQ through a [`BoundPlatformDevice`]
//! (so a failed probe releases everything via devres), and a presence gate that
//! tolerates the EC-gated `_STA` the Lenovo keyboard exposes.
//!
//! The actual controller/keyboard/mouse bring-up and the IRQ byte dispatch live
//! in [`crate::ps2`]; this module is the binding glue. The legacy hardcoded
//! bring-up still lives in [`crate::irq::init`] behind the `i8042.legacy`
//! escape hatch (see [`crate::ps2::legacy_mode`]); this driver declines when
//! that hatch is selected so the controller is never double-initialised.

use slopos_kernel_services::driver_runtime::{LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE};
use slopos_ostd::klog_info;

use crate::driver_core::BoundError;
use crate::driver_core::platform_bound::BoundPlatformDevice;
use crate::platform_bus::{
    MAX_PLATFORM_IO, PlatformDeviceInfo, PlatformIoWindow, PlatformIrq, PlatformMatch,
    PlatformProbeError, ProbeOutcome,
};
use crate::ps2;

/// Architectural i8042 data port (used when `_CRS` omits the I/O window).
const I8042_DATA_PORT: u16 = 0x60;
/// Architectural i8042 status/command port.
const I8042_CMD_PORT: u16 = 0x64;

crate::platform_driver! {
    pub static I8042_KBD = {
        name: "i8042-kbd",
        match_table: &[PlatformMatch::HidCid(b"PNP0303")],
        fallback: Some(i8042_fallback),
        probe: i8042_probe,
    };
}

/// Per-controller quirk decisions. Currently just whether to bring up the PS/2
/// AUX (mouse) port; a future per-machine quirk table would populate more here.
struct I8042Quirks {
    /// Whether a PS/2 AUX (mouse) device is present and should be initialised.
    aux: bool,
}

/// Probe a matched `PNP0303` device: presence-gate, claim resources, bring up
/// the controller + keyboard (+ mouse when present), and wire the IRQ lines.
fn i8042_probe(bound: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    // Legacy escape hatch: `irq::init` already performed the hardcoded bring-up,
    // so binding here would double-initialise the controller.
    if ps2::legacy_mode() {
        return Ok(ProbeOutcome::Declined);
    }

    let info = *bound.info();

    // Presence gate. An EC-gated `_STA` the narrow interpreter cannot evaluate
    // yields `present == None` (the Lenovo keyboard's `P2MK`) — that must NOT
    // decline. Only a positive "absent" rejects.
    if info.present == Some(false) {
        klog_info!("i8042: PNP0303 _STA reports absent; declining");
        return Ok(ProbeOutcome::Declined);
    }

    // Resource claim: reserve the controller's I/O ports (from `_CRS`, or the
    // architectural 0x60/0x64 when the template omitted them). Held by the
    // device's devres bag for ownership; released on probe failure / unbind.
    reserve_io_ports(bound, &info)?;

    // Controller + keyboard bring-up. The keyboard IOAPIC line is still masked
    // (it is programmed + unmasked by `request_legacy_irq` below), so the device
    // ACK / self-test bytes are polled here, not delivered as spurious IRQs.
    ps2::init_controller();
    ps2::keyboard::init();

    // AUX (PS/2 mouse): present on QEMU, absent on an I²C-HID laptop. A single
    // bounded reset probe avoids a multi-timeout stall on machines with no PS/2
    // pointing device.
    let quirks = I8042Quirks {
        aux: ps2::aux_reset_probe(),
    };
    if quirks.aux {
        ps2::mouse::init();
    } else {
        klog_info!("i8042: no PS/2 AUX device detected; skipping mouse bring-up");
    }

    ps2::flush();
    ps2::enable_irqs();

    // Wire IRQ1 (keyboard). The hardware-pinned legacy line comes from `_CRS`
    // (PNP0303 → IRQ 1); fall back to the architectural line if absent.
    let kbd_line = info.irq.map(|q| q.line).unwrap_or(LEGACY_IRQ_KEYBOARD);
    bound
        .request_legacy_irq(kbd_line, move |_ctx| ps2::dispatch_irq(kbd_line))
        .map_err(map_bound_err)?;

    if quirks.aux {
        bound
            .request_legacy_irq(LEGACY_IRQ_MOUSE, |_ctx| ps2::dispatch_irq(LEGACY_IRQ_MOUSE))
            .map_err(map_bound_err)?;
    }

    klog_info!(
        "i8042: bound PNP0303 (keyboard IRQ {}, aux={}, present={:?})",
        kbd_line,
        quirks.aux,
        info.present,
    );
    Ok(ProbeOutcome::Bound)
}

/// Reserve the controller's I/O ports for devres ownership.
fn reserve_io_ports(
    bound: &mut BoundPlatformDevice<'_>,
    info: &PlatformDeviceInfo,
) -> Result<(), PlatformProbeError> {
    let windows = info.io_ports();
    if windows.is_empty() {
        bound
            .reserve_io_port(I8042_DATA_PORT)
            .map_err(map_bound_err)?;
        bound
            .reserve_io_port(I8042_CMD_PORT)
            .map_err(map_bound_err)?;
        return Ok(());
    }
    for w in windows {
        let len = w.len.max(1) as u16;
        for off in 0..len {
            // A malformed firmware `_CRS` could place a window near 0xFFFF;
            // stop at the u16 boundary rather than overflow.
            let Some(port) = w.base.checked_add(off) else {
                break;
            };
            bound.reserve_io_port(port).map_err(map_bound_err)?;
        }
    }
    Ok(())
}

/// Synthesize an architectural i8042 device when ACPI enumeration found no
/// `PNP0303` node but the FADT advertises an 8042 (`IAPC_BOOT_ARCH` bit 1).
///
/// `present: None` so the probe's presence gate proceeds. This is the safety net
/// for firmware whose DSDT the narrow interpreter cannot resolve; when the
/// `PNP0303` node *is* discoverable the primary path binds and this is unused.
fn i8042_fallback(has_8042: bool) -> Option<PlatformDeviceInfo> {
    if !has_8042 {
        return None;
    }
    let mut io = [PlatformIoWindow::default(); MAX_PLATFORM_IO];
    io[0] = PlatformIoWindow {
        base: I8042_DATA_PORT,
        len: 1,
    };
    io[1] = PlatformIoWindow {
        base: I8042_CMD_PORT,
        len: 1,
    };
    Some(PlatformDeviceInfo {
        matched_id: b"PNP0303",
        io,
        io_count: 2,
        irq: Some(PlatformIrq {
            line: LEGACY_IRQ_KEYBOARD,
            edge: true,
            active_low: false,
        }),
        present: None,
    })
}

/// Map a resource-acquisition [`BoundError`] to a [`PlatformProbeError`].
fn map_bound_err(e: BoundError) -> PlatformProbeError {
    match e {
        BoundError::OutOfMemory => PlatformProbeError::OutOfMemory,
        _ => PlatformProbeError::Unsupported,
    }
}
