#![allow(dead_code)]

use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::io;
use slopos_lib::{cpu, interrupt_frame, kdiag_dump_interrupt_frame, klog_printf, tsc, KlogLevel};

use crate::{apic, ioapic, keyboard, wl_currency};

const IRQ_LINES: usize = 16;
const IRQ_BASE_VECTOR: u8 = 32;

const LEGACY_IRQ_TIMER: u8 = 0;
const LEGACY_IRQ_KEYBOARD: u8 = 1;
const LEGACY_IRQ_COM1: u8 = 4;

const PS2_DATA_PORT: u16 = 0x60;
const PS2_STATUS_PORT: u16 = 0x64;

type IrqHandler = unsafe extern "C" fn(u8, *mut interrupt_frame, *mut c_void);

#[derive(Clone, Copy)]
struct IrqEntry {
    handler: Option<IrqHandler>,
    context: *mut c_void,
    name: *const c_char,
    count: u64,
    last_timestamp: u64,
    masked: bool,
    reported_unhandled: bool,
}

impl IrqEntry {
    const fn new() -> Self {
        Self {
            handler: None,
            context: core::ptr::null_mut(),
            name: core::ptr::null(),
            count: 0,
            last_timestamp: 0,
            masked: true,
            reported_unhandled: false,
        }
    }
}

#[derive(Clone, Copy)]
struct IrqRouteState {
    via_ioapic: bool,
    gsi: u32,
}

impl IrqRouteState {
    const fn new() -> Self {
        Self {
            via_ioapic: false,
            gsi: 0,
        }
    }
}

static mut IRQ_TABLE: [IrqEntry; IRQ_LINES] = [IrqEntry::new(); IRQ_LINES];
static mut IRQ_ROUTE_TABLE: [IrqRouteState; IRQ_LINES] = [IrqRouteState::new(); IRQ_LINES];
static IRQ_SYSTEM_INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TIMER_TICK_COUNTER: u64 = 0;
static mut KEYBOARD_EVENT_COUNTER: u64 = 0;

extern "C" {
    fn kernel_panic(msg: *const c_char) -> !;
    fn scheduler_timer_tick();
    fn scheduler_handle_post_irq();
    fn scheduler_request_reschedule_from_interrupt();
}

#[inline]
fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

#[inline]
fn irq_line_has_ioapic_route(irq: u8) -> bool {
    if irq as usize >= IRQ_LINES {
        return false;
    }
    unsafe { IRQ_ROUTE_TABLE[irq as usize].via_ioapic }
}

#[inline]
fn acknowledge_irq() {
    apic::send_eoi();
}

fn mask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    unsafe {
        if !IRQ_TABLE[irq as usize].masked {
            if irq_line_has_ioapic_route(irq) {
                let _ = ioapic::mask_gsi(IRQ_ROUTE_TABLE[irq as usize].gsi);
            } else {
                log(
                    KlogLevel::Info,
                    b"IRQ: Mask request ignored for line (no IOAPIC route)\0",
                );
            }
            IRQ_TABLE[irq as usize].masked = true;
        }
    }
}

fn unmask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    unsafe {
        if !IRQ_TABLE[irq as usize].masked {
            return;
        }
        if irq_line_has_ioapic_route(irq) {
            let _ = ioapic::unmask_gsi(IRQ_ROUTE_TABLE[irq as usize].gsi);
            IRQ_TABLE[irq as usize].masked = false;
        } else {
            log(
                KlogLevel::Info,
                b"IRQ: Cannot unmask line (no IOAPIC route configured)\0",
            );
        }
    }
}

fn log_unhandled_irq(irq: u8, vector: u8) {
    unsafe {
        if irq as usize >= IRQ_LINES {
            klog_printf(
                KlogLevel::Info,
                b"IRQ: Spurious vector %u received\n\0".as_ptr() as *const c_char,
                vector as u32,
            );
            return;
        }

        let entry = &mut IRQ_TABLE[irq as usize];
        if entry.reported_unhandled {
            return;
        }
        entry.reported_unhandled = true;
        klog_printf(
            KlogLevel::Info,
            b"IRQ: Unhandled IRQ %u (vector %u) - masking line\n\0".as_ptr() as *const c_char,
            irq as u32,
            vector as u32,
        );
    }
}

unsafe extern "C" fn timer_irq_handler(irq: u8, _frame: *mut interrupt_frame, _ctx: *mut c_void) {
    (irq, _frame, _ctx);
    unsafe {
        TIMER_TICK_COUNTER = TIMER_TICK_COUNTER.wrapping_add(1);
        if TIMER_TICK_COUNTER <= 3 {
            klog_printf(
                KlogLevel::Debug,
                b"IRQ: Timer tick #%llu\n\0".as_ptr() as *const c_char,
                TIMER_TICK_COUNTER,
            );
        }
        scheduler_timer_tick();
    }
}

unsafe extern "C" fn keyboard_irq_handler(
    _irq: u8,
    _frame: *mut interrupt_frame,
    _ctx: *mut c_void,
) {
    unsafe {
        let status = io::inb(PS2_STATUS_PORT);
        if status & 0x01 == 0 {
            return;
        }

        let scancode = io::inb(PS2_DATA_PORT);
        KEYBOARD_EVENT_COUNTER = KEYBOARD_EVENT_COUNTER.wrapping_add(1);
        keyboard::keyboard_handle_scancode(scancode);
    }
}

fn irq_program_ioapic_route(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }

    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        unsafe {
            kernel_panic(
                b"IRQ: APIC/IOAPIC unavailable during route programming\0".as_ptr()
                    as *const c_char,
            )
        };
    }

    let mut gsi = 0u32;
    let mut legacy_flags = 0u32;
    if ioapic::legacy_irq_info(irq, &mut gsi, &mut legacy_flags) != 0 {
        unsafe { kernel_panic(b"IRQ: Failed to translate legacy IRQ\0".as_ptr() as *const c_char) };
    }

    let vector = IRQ_BASE_VECTOR.wrapping_add(irq) as u8;
    let lapic_id = apic::get_id() as u8;
    let flags = ioapic::IOAPIC_FLAG_DELIVERY_FIXED
        | ioapic::IOAPIC_FLAG_DEST_PHYSICAL
        | legacy_flags
        | ioapic::IOAPIC_FLAG_MASK;

    if ioapic::config_irq(gsi, vector, lapic_id, flags) != 0 {
        unsafe { kernel_panic(b"IRQ: Failed to program IOAPIC route\0".as_ptr() as *const c_char) };
    }

    unsafe {
        IRQ_ROUTE_TABLE[irq as usize].via_ioapic = true;
        IRQ_ROUTE_TABLE[irq as usize].gsi = gsi;
    }

    let polarity: *const c_char = if legacy_flags & ioapic::IOAPIC_FLAG_POLARITY_LOW != 0 {
        b"active-low\0".as_ptr() as *const c_char
    } else {
        b"active-high\0".as_ptr() as *const c_char
    };
    let trigger: *const c_char = if legacy_flags & ioapic::IOAPIC_FLAG_TRIGGER_LEVEL != 0 {
        b"level\0".as_ptr() as *const c_char
    } else {
        b"edge\0".as_ptr() as *const c_char
    };

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"IRQ: IOAPIC route IRQ %u -> GSI %u, vector 0x%x (%s, %s)\n\0".as_ptr()
                as *const c_char,
            irq as u32,
            gsi,
            vector as u32,
            polarity,
            trigger,
        );
    }

    unsafe {
        if IRQ_TABLE[irq as usize].masked {
            let _ = ioapic::mask_gsi(gsi);
        } else {
            let _ = ioapic::unmask_gsi(gsi);
        }
    }
}

fn irq_setup_ioapic_routes() {
    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        unsafe {
            kernel_panic(
                b"IRQ: APIC/IOAPIC not ready during dispatcher init\0".as_ptr() as *const c_char,
            )
        };
    }

    irq_program_ioapic_route(LEGACY_IRQ_TIMER);
    irq_program_ioapic_route(LEGACY_IRQ_KEYBOARD);
    irq_program_ioapic_route(LEGACY_IRQ_COM1);
}

#[no_mangle]
pub extern "C" fn irq_get_timer_ticks() -> u64 {
    unsafe { TIMER_TICK_COUNTER }
}

#[no_mangle]
pub extern "C" fn irq_init() {
    unsafe {
        for i in 0..IRQ_LINES {
            IRQ_TABLE[i] = IrqEntry::new();
            IRQ_ROUTE_TABLE[i] = IrqRouteState::new();
        }
        TIMER_TICK_COUNTER = 0;
        KEYBOARD_EVENT_COUNTER = 0;
    }

    IRQ_SYSTEM_INITIALIZED.store(true, Ordering::Relaxed);

    irq_setup_ioapic_routes();
    keyboard::keyboard_init();

    let _ = irq_register_handler(
        LEGACY_IRQ_TIMER,
        timer_irq_handler,
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = irq_register_handler(
        LEGACY_IRQ_KEYBOARD,
        keyboard_irq_handler,
        core::ptr::null_mut(),
        core::ptr::null(),
    );

    // Enable interrupts globally once IDT/APIC/IOAPIC routes and handlers are ready.
    cpu::enable_interrupts();
}

#[no_mangle]
pub extern "C" fn irq_register_handler(
    irq: u8,
    handler: IrqHandler,
    context: *mut c_void,
    name: *const c_char,
) -> i32 {
    if irq as usize >= IRQ_LINES {
        log(
            KlogLevel::Info,
            b"IRQ: Attempted to register handler for invalid line\0",
        );
        wl_currency::award_loss();
        return -1;
    }
    if handler as usize == 0 {
        log(
            KlogLevel::Info,
            b"IRQ: Attempted to register NULL handler\0",
        );
        wl_currency::award_loss();
        return -1;
    }

    unsafe {
        let entry = &mut IRQ_TABLE[irq as usize];
        entry.handler = Some(handler);
        entry.context = context;
        entry.name = name;
        entry.reported_unhandled = false;

        if !name.is_null() {
            klog_printf(
                KlogLevel::Debug,
                b"IRQ: Registered handler for line %u (%s)\n\0".as_ptr() as *const c_char,
                irq as u32,
                name,
            );
        } else {
            klog_printf(
                KlogLevel::Debug,
                b"IRQ: Registered handler for line %u\n\0".as_ptr() as *const c_char,
                irq as u32,
            );
        }
    }

    unmask_irq_line(irq);
    wl_currency::award_win();
    0
}

#[no_mangle]
pub extern "C" fn irq_unregister_handler(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    unsafe {
        let entry = &mut IRQ_TABLE[irq as usize];
        entry.handler = None;
        entry.context = core::ptr::null_mut();
        entry.name = core::ptr::null();
        entry.reported_unhandled = false;
    }
    mask_irq_line(irq);
    unsafe {
        klog_printf(
            KlogLevel::Debug,
            b"IRQ: Unregistered handler for line %u\n\0".as_ptr() as *const c_char,
            irq as u32,
        );
    }
}

#[no_mangle]
pub extern "C" fn irq_enable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    unsafe {
        IRQ_TABLE[irq as usize].reported_unhandled = false;
    }
    unmask_irq_line(irq);
}

#[no_mangle]
pub extern "C" fn irq_disable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    mask_irq_line(irq);
}

#[no_mangle]
pub extern "C" fn irq_dispatch(frame: *mut interrupt_frame) {
    if frame.is_null() {
        log(KlogLevel::Info, b"IRQ: Received null frame\0");
        return;
    }

    let frame_ref = unsafe { &mut *frame };
    let vector = (frame_ref.vector & 0xFF) as u8;
    let expected_cs = frame_ref.cs;
    let expected_rip = frame_ref.rip;

    if !IRQ_SYSTEM_INITIALIZED.load(Ordering::Relaxed) {
        log(
            KlogLevel::Info,
            b"IRQ: Dispatch received before initialization\0",
        );
        if vector >= IRQ_BASE_VECTOR {
            acknowledge_irq();
        }
        return;
    }

    if vector < IRQ_BASE_VECTOR {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"IRQ: Received non-IRQ vector %u\n\0".as_ptr() as *const c_char,
                vector as u32,
            );
        }
        return;
    }

    let irq = vector - IRQ_BASE_VECTOR;
    if irq as usize >= IRQ_LINES {
        log_unhandled_irq(0xFF, vector);
        acknowledge_irq();
        return;
    }

    let entry = unsafe { &mut IRQ_TABLE[irq as usize] };
    if entry.handler.is_none() {
        log_unhandled_irq(irq, vector);
        mask_irq_line(irq);
        acknowledge_irq();
        return;
    }

    entry.count = entry.count.wrapping_add(1);
    entry.last_timestamp = tsc::rdtsc();

    if let Some(handler) = entry.handler {
        unsafe { handler(irq, frame, entry.context) };
    }

    if frame_ref.cs != expected_cs || frame_ref.rip != expected_rip {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"IRQ: Frame corruption detected on IRQ %u - aborting\n\0".as_ptr()
                    as *const c_char,
                irq as u32,
            );
        }
        unsafe { kdiag_dump_interrupt_frame(frame) };
        unsafe { kernel_panic(b"IRQ: frame corrupted\0".as_ptr() as *const c_char) };
    }

    acknowledge_irq();
    unsafe { scheduler_handle_post_irq() };
}

#[repr(C)]
pub struct irq_stats {
    count: u64,
    last_timestamp: u64,
}

#[no_mangle]
pub extern "C" fn irq_get_stats(irq: u8, out_stats: *mut irq_stats) -> i32 {
    if irq as usize >= IRQ_LINES || out_stats.is_null() {
        return -1;
    }
    unsafe {
        (*out_stats).count = IRQ_TABLE[irq as usize].count;
        (*out_stats).last_timestamp = IRQ_TABLE[irq as usize].last_timestamp;
    }
    0
}
