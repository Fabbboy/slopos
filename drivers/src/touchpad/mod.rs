//! I²C-HID touchpad subsystem.
//!
//! Ties together ACPI discovery (`slopos_acpi::aml`), the I²C controller
//! ([`crate::i2c`]), the HID-over-I²C transport ([`i2c_hid`]), the report
//! parser ([`report`]), and the gesture engine ([`gesture`]). [`init`] is
//! called once from a boot step; it discovers the touchpad, brings it up,
//! and spawns a polling thread that feeds pointer events to the
//! compositor.
//!
//! Everything is failure-tolerant: with no I²C-HID touchpad present,
//! discovery returns nothing and the subsystem stays dormant.

pub mod gesture;
pub mod i2c_hid;
pub mod report;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_acpi::aml::{self, HhdmHost};
use slopos_acpi::tables::AcpiTables;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, OnceLock, SpinLock};
use slopos_ostd::{KArc, klog_info, klog_warn};

use crate::hpet;
use crate::i2c::{self, I2cBus};
use gesture::{Contact, Frame, GestureEngine, MAX_CONTACTS};
use i2c_hid::I2cHid;
use report::{
    PAGE_BUTTON, PAGE_DIGITIZER, PAGE_GENERIC_DESKTOP, ReportFormat, USAGE_BUTTON_1,
    USAGE_CONTACT_ID, USAGE_TIP_SWITCH, USAGE_X, USAGE_Y,
};

/// Poll interval for the (interrupt-less) input read loop.
const POLL_MS: u32 = 8;
/// Scheduler priority for the poll thread (mid-band).
const POLL_PRIORITY: u8 = 100;

struct TouchpadRuntime {
    hid: I2cHid,
    format: ReportFormat,
    gesture: SpinLock<GestureEngine>,
    debug: bool,
}

static TOUCHPAD: OnceLock<TouchpadRuntime> = OnceLock::new();

/// Discover and bring up the I²C-HID touchpad, then start polling.
/// `rsdp_phys` is the ACPI RSDP physical address; `width`/`height` are the
/// framebuffer dimensions for cursor bounds; `debug` enables verbose
/// bring-up tracing (`tp.debug=on`).
pub fn init(rsdp_phys: u64, width: u32, height: u32, debug: bool) {
    let Some(tables) = AcpiTables::from_phys(rsdp_phys) else {
        return;
    };
    let host = HhdmHost;
    let Some(found) = aml::scan_i2c_hid(&tables, &host, debug) else {
        klog_info!("touchpad: no I2C-HID device found in ACPI namespace");
        return;
    };
    klog_info!(
        "touchpad: ACPI I2C-HID ctrl_idx={} addr={:#04x} desc_reg={:#06x} speed={}Hz",
        found.controller_index,
        found.slave_addr,
        found.hid_desc_reg,
        found.speed_hz
    );

    let Some(bus) = controller_bus(found.controller_index) else {
        klog_warn!(
            "touchpad: I2C controller index {} not claimed by PCI probe",
            found.controller_index
        );
        return;
    };

    let hid = match I2cHid::bring_up(bus, found.slave_addr as u8, found.hid_desc_reg) {
        Ok(h) => h,
        Err(e) => {
            klog_warn!("touchpad: I2C-HID bring-up failed: {:?}", e);
            return;
        }
    };

    let rdesc = match hid.fetch_report_descriptor() {
        Ok(d) => d,
        Err(e) => {
            klog_warn!("touchpad: report descriptor fetch failed: {:?}", e);
            return;
        }
    };
    let format = report::parse_report_descriptor(rdesc.as_slice());

    // The device boots in mouse-compatibility mode (relative reports). If
    // it exposes the Input Mode selector, switch it to multitouch so the
    // absolute digitizer reports the gesture engine consumes start flowing.
    if let Some(rid) = format.input_mode_report_id {
        match hid.set_feature_report(rid, &[0x03]) {
            Ok(()) => {
                klog_info!("touchpad: requested multitouch mode (report {})", rid);
                hpet::delay_ms(50);
            }
            Err(e) => klog_warn!("touchpad: multitouch-mode request failed: {:?}", e),
        }
    }

    let (pad_x, pad_y) = pad_logical_max(&format);
    if debug {
        klog_info!(
            "touchpad: parsed {} input fields, pad_max=({},{}), report_ids={}",
            format.fields.len(),
            pad_x,
            pad_y,
            format.uses_report_ids
        );
    }
    if pad_x <= 1 || pad_y <= 1 {
        klog_warn!("touchpad: no usable X/Y digitizer fields; aborting");
        return;
    }

    let engine = GestureEngine::new(width as i32, height as i32, pad_x, pad_y);
    let rt = TouchpadRuntime {
        hid,
        format,
        gesture: SpinLock::new(engine, LOCK_LEVEL_RESOURCE),
        debug,
    };
    TOUCHPAD.call_once(move || rt);

    match slopos_ostd::task::spawn("touchpad-poll", poll_thread, POLL_PRIORITY) {
        Ok(_) => klog_info!("touchpad: ready (polling every {}ms)", POLL_MS),
        Err(e) => klog_warn!("touchpad: failed to spawn poll thread: {:?}", e),
    }
}

/// Update cursor bounds after a resolution change.
pub fn set_bounds(width: i32, height: i32) {
    if let Some(rt) = TOUCHPAD.get() {
        rt.gesture.lock().set_bounds(width, height);
    }
}

/// Bounded budget for per-poll diagnostic lines so a persistent read error
/// can't flood the kernel log ring buffer (which would corrupt a `cat`).
static POLL_LOG_BUDGET: AtomicU32 = AtomicU32::new(24);

fn poll_log_ok() -> bool {
    POLL_LOG_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if v > 0 { Some(v - 1) } else { None }
        })
        .is_ok()
}

/// The polling thread: read one input report, decode it, run the gesture
/// engine, sleep, repeat.
fn poll_thread() {
    let mut buf = [0u8; 256];
    loop {
        if let Some(rt) = TOUCHPAD.get() {
            match rt.hid.read_input_report(&mut buf) {
                Ok(n) if n > 0 => {
                    if let Some(frame) = extract_frame(&rt.format, &buf[..n]) {
                        let ts = timestamp_ms();
                        rt.gesture.lock().process(&frame, ts);
                    } else if rt.debug && poll_log_ok() {
                        klog_info!("touchpad: report of {} bytes, no decodable frame", n);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if rt.debug && poll_log_ok() {
                        klog_warn!("touchpad: read error {:?}", e);
                    }
                }
            }
        }
        hpet::delay_ms(POLL_MS);
    }
}

/// Map an ACPI I²C controller index (`I2Cn`) to the PCI-claimed bus.
/// Intel client PCHs place I²C 0–3 at device `0x15` and 4–5 at `0x19`.
fn controller_bus(index: u8) -> Option<KArc<I2cBus>> {
    let (device, function) = if index < 4 {
        (0x15u8, index)
    } else {
        (0x19u8, index - 4)
    };
    i2c::bus_by_bdf(0, device, function)
}

/// Logical extent of the touch surface. The descriptor also carries the
/// relative mouse collection (small signed range); take the largest X/Y
/// maximum so the absolute digitizer's extent wins.
fn pad_logical_max(format: &ReportFormat) -> (i32, i32) {
    let x = format
        .matches(PAGE_GENERIC_DESKTOP, USAGE_X)
        .map(|f| f.logical_max)
        .max()
        .unwrap_or(1);
    let y = format
        .matches(PAGE_GENERIC_DESKTOP, USAGE_Y)
        .map(|f| f.logical_max)
        .max()
        .unwrap_or(1);
    (x.max(1), y.max(1))
}

/// Decode an input report into a [`Frame`] of tipped contacts.
fn extract_frame(format: &ReportFormat, report: &[u8]) -> Option<Frame> {
    let rid = if format.uses_report_ids {
        *report.first()?
    } else {
        0
    };
    let data = if format.uses_report_ids {
        report.get(1..)?
    } else {
        report
    };

    let mut xs = [0i32; MAX_CONTACTS];
    let mut ys = [0i32; MAX_CONTACTS];
    let mut tips = [false; MAX_CONTACTS];
    let mut ids = [0u8; MAX_CONTACTS];
    let (mut nx, mut ny, mut nt, mut nid) = (0usize, 0usize, 0usize, 0usize);
    let mut button = false;

    for f in format.fields.iter().filter(|f| f.report_id == rid) {
        let raw = read_bits(data, f.bit_offset, f.bit_size);
        match (f.usage_page, f.usage) {
            (PAGE_GENERIC_DESKTOP, USAGE_X) if nx < MAX_CONTACTS => {
                xs[nx] = raw as i32;
                nx += 1;
            }
            (PAGE_GENERIC_DESKTOP, USAGE_Y) if ny < MAX_CONTACTS => {
                ys[ny] = raw as i32;
                ny += 1;
            }
            (PAGE_DIGITIZER, USAGE_TIP_SWITCH) if nt < MAX_CONTACTS => {
                tips[nt] = raw != 0;
                nt += 1;
            }
            (PAGE_DIGITIZER, USAGE_CONTACT_ID) if nid < MAX_CONTACTS => {
                ids[nid] = raw as u8;
                nid += 1;
            }
            (PAGE_BUTTON, USAGE_BUTTON_1) => button |= raw != 0,
            _ => {}
        }
    }

    let fingers = nx.min(ny).min(nt);
    let mut frame = Frame::empty();
    let mut count = 0;
    for i in 0..fingers {
        if tips[i] {
            frame.contacts[count] = Contact {
                id: *ids.get(i).unwrap_or(&(i as u8)),
                x: xs[i],
                y: ys[i],
                tip: true,
            };
            count += 1;
        }
    }
    frame.count = count;
    frame.button = button;
    Some(frame)
}

/// Read `bit_size` (≤32) bits at `bit_offset` from `data`, little-endian.
fn read_bits(data: &[u8], bit_offset: u32, bit_size: u32) -> u32 {
    let mut v = 0u32;
    let n = bit_size.min(32);
    for i in 0..n {
        let bit = bit_offset + i;
        let byte = (bit / 8) as usize;
        let shift = (bit % 8) as u32;
        if let Some(&b) = data.get(byte) {
            if b & (1 << shift) != 0 {
                v |= 1 << i;
            }
        }
    }
    v
}

fn timestamp_ms() -> u64 {
    hpet::nanoseconds(hpet::read_counter()) / 1_000_000
}
