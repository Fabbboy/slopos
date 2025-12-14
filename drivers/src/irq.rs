#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::io;
use slopos_lib::{cpu, interrupt_frame, kdiag_dump_interrupt_frame, klog_printf, tsc, KlogLevel};
use slopos_lib::spinlock::Spinlock;

use crate::{apic, ioapic, keyboard, wl_currency};

const IRQ_LINES: usize = 16;
const IRQ_BASE_VECTOR: u8 = 32;

const LEGACY_IRQ_TIMER: u8 = 0;
const LEGACY_IRQ_KEYBOARD: u8 = 1;
const LEGACY_IRQ_COM1: u8 = 4;

const PS2_DATA_PORT: u16 = 0x60;
const PS2_STATUS_PORT: u16 = 0x64;

type IrqHandler = extern "C" fn(u8, *mut interrupt_frame, *mut c_void);

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

struct IrqTables {
    entries: UnsafeCell<[IrqEntry; IRQ_LINES]>,
    routes: UnsafeCell<[IrqRouteState; IRQ_LINES]>,
}

unsafe impl Sync for IrqTables {}

impl IrqTables {
    const fn new() -> Self {
        Self {
            entries: UnsafeCell::new([IrqEntry::new(); IRQ_LINES]),
            routes: UnsafeCell::new([IrqRouteState::new(); IRQ_LINES]),
        }
    }

    fn entries_mut(&self) -> *mut [IrqEntry; IRQ_LINES] {
        self.entries.get()
    }

    fn routes_mut(&self) -> *mut [IrqRouteState; IRQ_LINES] {
        self.routes.get()
    }
}

static IRQ_TABLES: IrqTables = IrqTables::new();
static IRQ_SYSTEM_INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TIMER_TICK_COUNTER: u64 = 0;
static mut KEYBOARD_EVENT_COUNTER: u64 = 0;
static IRQ_TABLE_LOCK: Spinlock = Spinlock::new();

unsafe extern "C" {
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
fn with_irq_tables<R>(
    f: impl FnOnce(&mut [IrqEntry; IRQ_LINES], &mut [IrqRouteState; IRQ_LINES]) -> R,
) -> R {
    let flags = IRQ_TABLE_LOCK.lock_irqsave();
    let res = unsafe {
        f(
            &mut *IRQ_TABLES.entries_mut(),
            &mut *IRQ_TABLES.routes_mut(),
        )
    };
    IRQ_TABLE_LOCK.unlock_irqrestore(flags);
    res
}

#[inline]
fn acknowledge_irq() {
    apic::send_eoi();
}

fn mask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    let (mask_hw, gsi) = with_irq_tables(|table, routes| {
        if table[irq as usize].masked {
            return (false, 0);
        }
        table[irq as usize].masked = true;
        if routes[irq as usize].via_ioapic {
            (true, routes[irq as usize].gsi)
        } else {
            (false, 0)
        }
    });
    if mask_hw {
        let _ = ioapic::mask_gsi(gsi);
    } else {
        log(
            KlogLevel::Info,
            b"IRQ: Mask request ignored for line (no IOAPIC route)\0",
        );
    }
}

fn unmask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    let (unmask_hw, gsi) = with_irq_tables(|table, routes| {
        if !table[irq as usize].masked {
            return (false, 0);
        }
        table[irq as usize].masked = false;
        if routes[irq as usize].via_ioapic {
            (true, routes[irq as usize].gsi)
        } else {
            (false, 0)
        }
    });
    if unmask_hw {
        let _ = ioapic::unmask_gsi(gsi);
    } else {
        log(
            KlogLevel::Info,
            b"IRQ: Cannot unmask line (no IOAPIC route configured)\0",
        );
    }
}

fn log_unhandled_irq(irq: u8, vector: u8) {
    if irq as usize >= IRQ_LINES {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"IRQ: Spurious vector %u received\n\0".as_ptr() as *const c_char,
                vector as u32,
            );
        }
        return;
    }

    let already_reported = with_irq_tables(|table, _| {
        let entry = &mut table[irq as usize];
        if entry.reported_unhandled {
            true
        } else {
            entry.reported_unhandled = true;
            false
        }
    });
    if already_reported {
        return;
    }
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"IRQ: Unhandled IRQ %u (vector %u) - masking line\n\0".as_ptr() as *const c_char,
            irq as u32,
            vector as u32,
        );
    }
}

extern "C" fn timer_irq_handler(irq: u8, _frame: *mut interrupt_frame, _ctx: *mut c_void) {
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

extern "C" fn keyboard_irq_handler(
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

    let masked = with_irq_tables(|table, routes| {
        routes[irq as usize].via_ioapic = true;
        routes[irq as usize].gsi = gsi;
        table[irq as usize].masked
    });

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

    if masked {
        let _ = ioapic::mask_gsi(gsi);
    } else {
        let _ = ioapic::unmask_gsi(gsi);
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

#[unsafe(no_mangle)]
pub extern "C" fn irq_get_timer_ticks() -> u64 {
    unsafe { TIMER_TICK_COUNTER }
}

#[unsafe(no_mangle)]
pub extern "C" fn irq_init() {
    with_irq_tables(|table, routes| {
        for i in 0..IRQ_LINES {
            table[i] = IrqEntry::new();
            routes[i] = IrqRouteState::new();
        }
    });
    unsafe {
        TIMER_TICK_COUNTER = 0;
        KEYBOARD_EVENT_COUNTER = 0;
    }

    IRQ_SYSTEM_INITIALIZED.store(true, Ordering::Relaxed);

    irq_setup_ioapic_routes();
    keyboard::keyboard_init();

    let _ = irq_register_handler(
        LEGACY_IRQ_TIMER,
        Some(timer_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = irq_register_handler(
        LEGACY_IRQ_KEYBOARD,
        Some(keyboard_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );

    // Enable interrupts globally once IDT/APIC/IOAPIC routes and handlers are ready.
    cpu::enable_interrupts();
}

#[unsafe(no_mangle)]
pub extern "C" fn irq_register_handler(
    irq: u8,
    handler: Option<IrqHandler>,
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

    with_irq_tables(|table, _| {
        let entry = &mut table[irq as usize];
        entry.handler = handler;
        entry.context = context;
        entry.name = name;
        entry.reported_unhandled = false;
    });

    unsafe {
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

#[unsafe(no_mangle)]
pub extern "C" fn irq_unregister_handler(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    with_irq_tables(|table, _| {
        let entry = &mut table[irq as usize];
        entry.handler = None;
        entry.context = core::ptr::null_mut();
        entry.name = core::ptr::null();
        entry.reported_unhandled = false;
    });
    mask_irq_line(irq);
    unsafe {
        klog_printf(
            KlogLevel::Debug,
            b"IRQ: Unregistered handler for line %u\n\0".as_ptr() as *const c_char,
            irq as u32,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn irq_enable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    with_irq_tables(|table, _| {
        table[irq as usize].reported_unhandled = false;
    });
    unmask_irq_line(irq);
}

#[unsafe(no_mangle)]
pub extern "C" fn irq_disable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    mask_irq_line(irq);
}

#[unsafe(no_mangle)]
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

    let handler_snapshot = with_irq_tables(|table, _| {
        let entry = &mut table[irq as usize];
        if entry.handler.is_none() {
            return None;
        }
        entry.count = entry.count.wrapping_add(1);
        entry.last_timestamp = tsc::rdtsc();
        entry.handler.map(|h| (h, entry.context))
    });

    let Some((handler, context)) = handler_snapshot else {
        log_unhandled_irq(irq, vector);
        mask_irq_line(irq);
        acknowledge_irq();
        return;
    };

    handler(irq, frame, context);

    if frame_ref.cs != expected_cs || frame_ref.rip != expected_rip {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"IRQ: Frame corruption detected on IRQ %u - aborting\n\0".as_ptr()
                    as *const c_char,
                irq as u32,
            );
        }
        kdiag_dump_interrupt_frame(frame);
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

#[unsafe(no_mangle)]
pub extern "C" fn irq_get_stats(irq: u8, out_stats: *mut irq_stats) -> i32 {
    if irq as usize >= IRQ_LINES || out_stats.is_null() {
        return -1;
    }
    with_irq_tables(|table, _| {
        unsafe {
            (*out_stats).count = table[irq as usize].count;
            (*out_stats).last_timestamp = table[irq as usize].last_timestamp;
        }
    });
    0
}
