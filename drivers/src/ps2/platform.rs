//! The i8042 PS/2 controller as a platform (ACPI) device driver: binds the ACPI
//! `PNP0303` node through [`crate::platform_bus`], claiming its 0x60/0x64 ports
//! and legacy IRQ under devres. Declines when the `i8042.legacy` escape hatch is
//! selected, so the controller is never double-initialised.

use slopos_kernel_services::driver_runtime::{LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE};
use slopos_ostd::klog_info;

use crate::driver_core::BoundError;
use crate::platform_bus::BoundPlatformDevice;
use crate::platform_bus::{
    MAX_PLATFORM_IO, PlatformDeviceInfo, PlatformIoWindow, PlatformIrq, PlatformMatch,
    PlatformProbeError, ProbeOutcome,
};
use crate::ps2;

/// Architectural ports, used when `_CRS` omits the I/O window.
const I8042_DATA_PORT: u16 = 0x60;
const I8042_CMD_PORT: u16 = 0x64;

crate::platform_driver! {
    pub static I8042_KBD = {
        name: "i8042-kbd",
        match_table: &[PlatformMatch::HidCid(b"PNP0303")],
        fallback: Some(i8042_fallback),
        probe: i8042_probe,
    };
}

struct I8042Quirks {
    aux: bool,
}

fn i8042_probe(bound: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    // `irq::init` already ran the hardcoded bring-up; binding would repeat it.
    if ps2::legacy_mode() {
        return Ok(ProbeOutcome::Declined);
    }

    let info = *bound.info();

    // An EC-gated `_STA` the narrow interpreter cannot evaluate yields `None`,
    // so only a positive "absent" may decline.
    if info.present == Some(false) {
        klog_info!("i8042: PNP0303 _STA reports absent; declining");
        return Ok(ProbeOutcome::Declined);
    }

    reserve_io_ports(bound, &info)?;

    // The keyboard IOAPIC line stays masked until `request_legacy_irq` below, so
    // ACK and self-test bytes are polled here instead of arriving as spurious IRQs.
    ps2::init_controller();
    ps2::keyboard::init();

    // One bounded reset probe: a machine with no PS/2 pointing device would
    // otherwise stall through several timeouts.
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
            // A malformed `_CRS` could place a window near 0xFFFF.
            let Some(port) = w.base.checked_add(off) else {
                break;
            };
            bound.reserve_io_port(port).map_err(map_bound_err)?;
        }
    }
    Ok(())
}

/// Synthesize an architectural i8042 when ACPI found no `PNP0303` node but the
/// FADT advertises an 8042 (`IAPC_BOOT_ARCH` bit 1). `present: None` so the
/// probe's presence gate proceeds.
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

fn map_bound_err(e: BoundError) -> PlatformProbeError {
    match e {
        BoundError::OutOfMemory => PlatformProbeError::OutOfMemory,
        _ => PlatformProbeError::Unsupported,
    }
}
