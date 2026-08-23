//! I²C-HID touchpad subsystem: ACPI discovery, HID-over-I²C transport, report
//! parsing and gesture generation, wired up by [`init`] from a boot step.
//!
//! Input is interrupt-driven when the device's GpioInt can be wired through the
//! PCH pinctrl controller ([`crate::pinctrl`]); otherwise a polling thread drains
//! reports.

pub mod gesture;
pub mod i2c_hid;
pub mod report;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use slopos_ostd::lock_class;

use slopos_acpi::aml::{self, AcpiI2cHid, HhdmHost};
use slopos_acpi::tables::AcpiTables;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, OnceLock, SpinLock};
use slopos_ostd::{KArc, klog_info, klog_warn};

use crate::hpet;
use crate::i2c::{self, I2cBus};
use crate::pinctrl;
use gesture::{Contact, Frame, GestureEngine, MAX_CONTACTS};
use i2c_hid::I2cHid;
use report::{
    PAGE_BUTTON, PAGE_DIGITIZER, PAGE_GENERIC_DESKTOP, ReportFormat, USAGE_BUTTON_1,
    USAGE_CONTACT_ID, USAGE_TIP_SWITCH, USAGE_X, USAGE_Y,
};

/// Poll interval for the fallback (interrupt-less) input read loop.
const POLL_MS: u32 = 8;

struct TouchpadRuntime {
    hid: I2cHid,
    format: ReportFormat,
    gesture: SpinLock<GestureEngine>,
    debug: bool,
}

static TOUCHPAD: OnceLock<TouchpadRuntime> = OnceLock::new();

/// Discover and bring up the I²C-HID touchpad, then start polling.
/// `width`/`height` bound the cursor; `debug` (`tp.debug=on`) traces bring-up.
pub fn init(rsdp_phys: u64, width: u32, height: u32, debug: bool, force_poll: bool) {
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

    // The device boots in mouse-compatibility mode (relative reports); `0x03`
    // selects multitouch so the absolute digitizer reports start flowing.
    if let Some(rid) = format.input_mode_report_id {
        match hid.set_feature_report(rid, &[0x03]) {
            Ok(()) => {
                klog_info!("touchpad: requested multitouch mode (report {})", rid);
                hpet::delay_ms(50);
            }
            Err(e) => klog_warn!("touchpad: multitouch-mode request failed: {:?}", e),
        }
    } else if debug {
        klog_info!("touchpad: no input-mode selector; device stays in mouse mode");
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
        gesture: SpinLock::new(engine, lock_class!("TOUCHPAD", LOCK_LEVEL_RESOURCE)),
        debug,
    };
    TOUCHPAD.call_once(move || rt);

    if try_interrupt_mode(&found, force_poll) {
        return;
    }
    match slopos_ostd::spawn_kernel_io!(&POLL_STOP, poll_thread) {
        Ok(_) => klog_info!("touchpad: ready (polling every {}ms)", POLL_MS),
        Err(e) => klog_warn!("touchpad: failed to spawn poll thread: {:?}", e),
    }
}

/// IO-APIC line the Intel PCH GPIO controller (`INTC1055`) funnels pad interrupts
/// onto on Alder Lake-P; the `_CRS` that would name it sits in an OperationRegion
/// the narrow AML reader can't resolve.
const GPIO_DEFAULT_GSI: u32 = 14;

/// IRQ-armed waker: the GPIO ISR signals it, the drain thread parks on it.
/// `WaitQueue::wake_*` is IRQ-safe; the armed flag closes the wake/park race.
struct TouchpadWaker {
    armed: AtomicBool,
    stop: KernelIoStop,
}

impl TouchpadWaker {
    const fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            stop: KernelIoStop::new(
                "touchpad-irq",
                lock_class!("TOUCHPAD_IRQ_STOP.waiters", LOCK_LEVEL_RESOURCE),
            ),
        }
    }
    const fn stop(&self) -> &KernelIoStop {
        &self.stop
    }
    fn arm_and_wake(&self) {
        self.armed.store(true, Ordering::Release);
        self.stop.wake_for_work();
    }
    /// Park until armed or a stop is requested; consumes one edge.
    fn wait(&self, token: &KernelIoToken<'_>) -> KthreadWait {
        token.park(&self.stop, || {
            self.armed
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
    }
}

static TOUCHPAD_WAKER: TouchpadWaker = TouchpadWaker::new();

static POLL_STOP: KernelIoStop = KernelIoStop::new(
    "touchpad-poll",
    lock_class!("TOUCHPAD_POLL_STOP.waiters", LOCK_LEVEL_RESOURCE),
);

/// Returns `true` if interrupt-driven input was engaged.
fn try_interrupt_mode(found: &AcpiI2cHid, force_poll: bool) -> bool {
    if force_poll {
        return false;
    }
    let Some(line) = found.gpio_int_pin else {
        return false;
    };
    let Some(pin) = pinctrl::init_for_pad(line, found.gpio_int_edge, found.gpio_int_active_low)
    else {
        klog_warn!(
            "touchpad: pinctrl setup failed for GpioInt {}; polling",
            line
        );
        return false;
    };

    // Intel GPIO controller cascade parent: level-triggered, active-low.
    if !register_cascade(GPIO_DEFAULT_GSI, false, true) {
        klog_warn!(
            "touchpad: GSI {} cascade wiring failed; polling",
            GPIO_DEFAULT_GSI
        );
        return false;
    }
    if let Err(e) = slopos_ostd::spawn_kernel_io!(TOUCHPAD_WAKER.stop(), irq_thread) {
        klog_warn!("touchpad: failed to spawn irq thread: {:?}; polling", e);
        return false;
    }
    pinctrl::pad_irq_unmask();
    klog_info!(
        "touchpad: interrupt-driven (pin {}, GSI {}, padcfg0={:#010x})",
        pin,
        GPIO_DEFAULT_GSI,
        pinctrl::padcfg0_snapshot().unwrap_or(0)
    );
    true
}

/// Route the cascade GSI and register the demux ISR. Leaks the line and handle
/// (kernel-lifetime registration); returns `false` on any failure.
fn register_cascade(gsi: u32, edge: bool, active_low: bool) -> bool {
    use crate::ioapic::regs::{
        IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
        IOAPIC_FLAG_POLARITY_HIGH, IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_EDGE,
        IOAPIC_FLAG_TRIGGER_LEVEL,
    };
    use slopos_ostd::irq::{IrqAllocator, IrqContext};

    if !crate::apic::is_enabled() || crate::ioapic::is_ready() == 0 {
        return false;
    }
    let line = match IrqAllocator::alloc() {
        Ok(l) => l,
        Err(_) => return false,
    };
    let vector = line.vector();
    let lapic_id = crate::apic::get_id() as u8;
    let mut flags = IOAPIC_FLAG_DELIVERY_FIXED | IOAPIC_FLAG_DEST_PHYSICAL | IOAPIC_FLAG_MASK;
    flags |= if edge {
        IOAPIC_FLAG_TRIGGER_EDGE
    } else {
        IOAPIC_FLAG_TRIGGER_LEVEL
    };
    flags |= if active_low {
        IOAPIC_FLAG_POLARITY_LOW
    } else {
        IOAPIC_FLAG_POLARITY_HIGH
    };
    if crate::ioapic::config_irq(gsi, vector, lapic_id, flags) != 0 {
        return false;
    }
    let handle = match line.register_callback(|_ctx: &IrqContext<'_>| {
        if pinctrl::service_pending() {
            TOUCHPAD_WAKER.arm_and_wake();
        }
        crate::apic::send_eoi();
    }) {
        Ok(h) => h,
        Err(_) => return false,
    };
    // Forget the handle before the line: it borrows `line`.
    core::mem::forget(handle);
    core::mem::forget(line);
    crate::ioapic::unmask_gsi(gsi);
    true
}

/// Interrupt drain thread: parks until the GPIO ISR signals, then reads every
/// pending report — draining them is what de-asserts the device's DRDY line.
fn irq_thread(token: KernelIoToken<'static>) {
    let mut buf = [0u8; 256];
    let mut first_report = true;
    loop {
        if TOUCHPAD_WAKER.wait(&token) == KthreadWait::Stop {
            break;
        }
        if let Some(rt) = TOUCHPAD.get() {
            loop {
                match rt.hid.read_input_report(&mut buf) {
                    Ok(n) if n > 0 => {
                        if rt.debug && first_report {
                            first_report = false;
                            klog_info!(
                                "touchpad: first report ({} bytes): {:02x?}",
                                n,
                                &buf[..n.min(24)]
                            );
                        }
                        if let Some(frame) = extract_frame(&rt.format, &buf[..n]) {
                            let ts = timestamp_ms();
                            rt.gesture.lock().process(&frame, ts);
                        }
                    }
                    Ok(_) => break,
                    Err(e) => {
                        if rt.debug && poll_log_ok() {
                            klog_warn!("touchpad: irq read error {:?}", e);
                        }
                        break;
                    }
                }
            }
        }
        pinctrl::pad_irq_unmask();
    }
    // Leave the pad masked: the forgotten IRQ handle keeps the line configured, so
    // a late edge would assert an interrupt with nobody left to drain it.
    pinctrl::pad_irq_mask();
    TOUCHPAD_WAKER.stop().note_exited();
}

/// Update cursor bounds after a resolution change.
pub fn set_bounds(width: i32, height: i32) {
    if let Some(rt) = TOUCHPAD.get() {
        rt.gesture.lock().set_bounds(width, height);
    }
}

/// Budget for per-poll diagnostic lines so a persistent read error can't flood
/// the kernel log ring.
static POLL_LOG_BUDGET: AtomicU32 = AtomicU32::new(24);

fn poll_log_ok() -> bool {
    POLL_LOG_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if v > 0 { Some(v - 1) } else { None }
        })
        .is_ok()
}

fn poll_thread(token: KernelIoToken<'static>) {
    let mut buf = [0u8; 256];
    // `tp.debug` counters separate the failure stages: `data` stuck at 0 means the
    // device produces nothing, `tipped` stuck at 0 means mouse-compat mode.
    let mut first_report = true;
    let mut first_tip = true;
    let (mut n_empty, mut n_data, mut n_err, mut n_tipped) = (0u64, 0u64, 0u64, 0u64);
    let mut polls = 0u64;
    loop {
        if let Some(rt) = TOUCHPAD.get() {
            match rt.hid.read_input_report(&mut buf) {
                Ok(n) if n > 0 => {
                    n_data += 1;
                    if rt.debug && first_report {
                        first_report = false;
                        klog_info!(
                            "touchpad: first report ({} bytes): {:02x?}",
                            n,
                            &buf[..n.min(24)]
                        );
                    }
                    if let Some(frame) = extract_frame(&rt.format, &buf[..n]) {
                        if frame.count > 0 {
                            n_tipped += 1;
                            if rt.debug && first_tip {
                                first_tip = false;
                                klog_info!(
                                    "touchpad: first contact frame: count={} x={} y={} button={}",
                                    frame.count,
                                    frame.contacts[0].x,
                                    frame.contacts[0].y,
                                    frame.button
                                );
                            }
                        }
                        let ts = timestamp_ms();
                        rt.gesture.lock().process(&frame, ts);
                    } else if rt.debug && poll_log_ok() {
                        klog_info!("touchpad: report of {} bytes, no decodable frame", n);
                    }
                }
                Ok(_) => n_empty += 1,
                Err(e) => {
                    n_err += 1;
                    if rt.debug && poll_log_ok() {
                        klog_warn!("touchpad: read error {:?}", e);
                    }
                }
            }
            polls += 1;
            // Own cap, so a burst of read-error lines can't starve the heartbeat.
            if rt.debug && polls % 512 == 0 && polls <= 512 * 24 {
                klog_info!(
                    "touchpad: poll stats: empty={} data={} err={} tipped={}",
                    n_empty,
                    n_data,
                    n_err,
                    n_tipped
                );
            }
        }
        // A park rather than a bare sleep, so the wait is also where a stop or
        // a freeze is observed.
        if token.park_timeout(&POLL_STOP, || false, POLL_MS as u64) == KthreadWait::Stop {
            break;
        }
    }
    POLL_STOP.note_exited();
}

/// Intel client PCHs place I²C 0–3 at device `0x15` and 4–5 at `0x19`.
fn controller_bus(index: u8) -> Option<KArc<I2cBus>> {
    let (device, function) = if index < 4 {
        (0x15u8, index)
    } else {
        (0x19u8, index - 4)
    };
    i2c::bus_by_bdf(0, device, function)
}

/// Logical extent of the touch surface. The descriptor also carries the relative
/// mouse collection, so the largest X/Y maximum is the absolute digitizer's.
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

// A touchpad's GpioInt pin is patched in by `_INI` as `pin = (enc & 0xFFFF) +
// PADTABLE[group][col]`; this exercises that shape.

/// AML host for the evaluator test: no `SystemMemory` fields are read, so this
/// never gets called.
struct ZeroHost;
impl slopos_acpi::aml::AmlHost for ZeroHost {
    fn read_phys(&self, _phys: u64, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = 0;
        }
    }
}

#[doc(hidden)]
pub fn test_aml_method_package_eval() -> slopos_testing::TestResult {
    // Name (GPLT, Package(2){ Package(2){0x10,0xA0}, Package(2){0x30,0x40} })
    // Method (GINF, 2) { Return (DerefOf(DerefOf(GPLT[Arg0])[Arg1])) }
    // Method (GTST, 1) { Return (Add(And(Arg0, 0x0F), GINF(1, 1))) }
    // Method (GPLT, 0) { Return (Zero) }  — a same-named method the package
    //   must win over (a flat namespace would otherwise call it → 0).
    // GTST(0x35) = (0x35 & 0x0F) + GPLT[1][1] = 5 + 0x40 = 69.
    #[rustfmt::skip]
    let aml: [u8; 72] = [
        // Name(GPLT, Package{...})
        0x08, 0x47, 0x50, 0x4C, 0x54, // Name "GPLT"
        0x12, 0x10, 0x02, // Package, len 16, 2 elements
        0x12, 0x06, 0x02, 0x0a, 0x10, 0x0a, 0xa0, // {0x10, 0xA0}
        0x12, 0x06, 0x02, 0x0a, 0x30, 0x0a, 0x40, // {0x30, 0x40}
        // Method(GINF, 2) { Return(DerefOf(DerefOf(GPLT[Arg0])[Arg1])) }
        0x14, 0x13, 0x47, 0x49, 0x4E, 0x46, 0x02, // Method "GINF", argc 2
        0xa4, 0x83, 0x88, 0x83, 0x88, 0x47, 0x50, 0x4C, 0x54, 0x68, 0x00, 0x69, 0x00,
        // Method(GTST, 1) { Return(Add(And(Arg0,0x0F), GINF(1,1))) }
        0x14, 0x14, 0x47, 0x54, 0x53, 0x54, 0x01, // Method "GTST", argc 1
        0xa4, 0x72, 0x7b, 0x68, 0x0a, 0x0f, 0x00, 0x47, 0x49, 0x4E, 0x46, 0x01, 0x01, 0x00,
        // Method(GPLT, 0) { Return(Zero) } — name collision with the package
        0x14, 0x08, 0x47, 0x50, 0x4C, 0x54, 0x00, 0xa4, 0x00,
    ];
    match slopos_acpi::aml::eval_method_u64_for_test(&aml, &ZeroHost, b"GTST", &[0x35]) {
        Some(69) => slopos_testing::TestResult::Pass,
        _ => slopos_testing::TestResult::Fail,
    }
}

slopos_testing::stest!(name = test_aml_method_package_eval, suite = touchpad);

// A method body whose *first* statement is a Type2 op writing its Target
// (`ShiftRight(And(..), .., Local0)`) must execute and reach its `Return`.
// This is the Intel GPIO `GGRP`; returning 0 picks the wrong package row.
#[doc(hidden)]
pub fn test_aml_target_op_statement() -> slopos_testing::TestResult {
    // Name(GPCL, Package(2){ Package(2){0xAA,0x11}, Package(2){0xBB,0xA0} })
    //   GPCL[0][1]=0x11 (the wrong row), GPCL[1][1]=0xA0=160 (the right one).
    // Method(GINF,2) { Return(DerefOf(DerefOf(GPCL[Arg0])[Arg1])) }
    // Method(GGRP,1) { ShiftRight(And(Arg0,0x00FF0000),0x10,Local0); Return(Local0) }
    // Method(GNMB,1) { Return(And(Arg0,0xFFFF)) }
    // Method(GNUM,1) { Local0=GNMB(Arg0); Local1=GGRP(Arg0);
    //                  Return(GINF(Local1, One) + Local0) }
    // enc=0x00010005 → GGRP=1, GNMB=5 → GNUM = GPCL[1][1] + 5 = 160 + 5 = 165.
    #[rustfmt::skip]
    let aml = [
        // Name(GPCL, Package{ {0xAA,0x11}, {0xBB,0xA0} })
        0x08, 0x47, 0x50, 0x43, 0x4C,
        0x12, 0x10, 0x02,
        0x12, 0x06, 0x02, 0x0a, 0xAA, 0x0a, 0x11,
        0x12, 0x06, 0x02, 0x0a, 0xBB, 0x0a, 0xA0,
        // Method(GINF, 2) { Return(DerefOf(DerefOf(GPCL[Arg0])[Arg1])) }
        0x14, 0x13, 0x47, 0x49, 0x4E, 0x46, 0x02,
        0xa4, 0x83, 0x88, 0x83, 0x88, 0x47, 0x50, 0x43, 0x4C, 0x68, 0x00, 0x69, 0x00,
        // Method(GGRP, 1) { ShiftRight(And(Arg0,0x00FF0000),0x10,Local0); Return(Local0) }
        0x14, 0x14, 0x47, 0x47, 0x52, 0x50, 0x01,
        0x7a, 0x7b, 0x68, 0x0c, 0x00, 0x00, 0xff, 0x00, 0x00, 0x0a, 0x10, 0x60,
        0xa4, 0x60,
        // Method(GNMB, 1) { Return(And(Arg0, 0xFFFF)) }
        0x14, 0x0d, 0x47, 0x4E, 0x4D, 0x42, 0x01,
        0xa4, 0x7b, 0x68, 0x0b, 0xff, 0xff, 0x00,
        // Method(GNUM, 1) { Local0=GNMB(Arg0); Local1=GGRP(Arg0);
        //                   Return(GINF(Local1, One) + Local0) }
        0x14, 0x1e, 0x47, 0x4E, 0x55, 0x4D, 0x01,
        0x70, 0x47, 0x4E, 0x4D, 0x42, 0x68, 0x60,
        0x70, 0x47, 0x47, 0x52, 0x50, 0x68, 0x61,
        0xa4, 0x72, 0x47, 0x49, 0x4E, 0x46, 0x61, 0x01, 0x60, 0x00,
    ];
    // GGRP alone: the leading Target-op statement executes (→ 1).
    if slopos_acpi::aml::eval_method_u64_for_test(&aml, &ZeroHost, b"GGRP", &[0x0001_0005])
        != Some(1)
    {
        return slopos_testing::TestResult::Fail;
    }
    match slopos_acpi::aml::eval_method_u64_for_test(&aml, &ZeroHost, b"GNUM", &[0x0001_0005]) {
        Some(165) => slopos_testing::TestResult::Pass,
        _ => slopos_testing::TestResult::Fail,
    }
}

slopos_testing::stest!(name = test_aml_target_op_statement, suite = touchpad);
