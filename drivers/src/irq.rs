use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::io;
use slopos_lib::spinlock::Spinlock;
use slopos_lib::{InterruptFrame, cpu, kdiag_dump_interrupt_frame, klog_debug, klog_info, tsc};

use crate::{apic, ioapic, keyboard, mouse, sched_bridge, wl_currency};
use slopos_abi::arch::x86_64::ioapic::{
    IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
    IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_LEVEL,
};
use slopos_abi::arch::x86_64::ports::{PS2_DATA_PORT, PS2_STATUS_PORT};

use slopos_abi::arch::IRQ_BASE_VECTOR;

const IRQ_LINES: usize = 16;

const LEGACY_IRQ_TIMER: u8 = 0;
const LEGACY_IRQ_KEYBOARD: u8 = 1;
const LEGACY_IRQ_COM1: u8 = 4;
const LEGACY_IRQ_MOUSE: u8 = 12;

type IrqHandler = extern "C" fn(u8, *mut InterruptFrame, *mut c_void);

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
        klog_info!("IRQ: Mask request ignored for line (no IOAPIC route)");
    }
}

fn unmask_irq_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    let (unmask_hw, gsi, was_masked) = with_irq_tables(|table, routes| {
        if !table[irq as usize].masked {
            return (false, 0, false);
        }
        table[irq as usize].masked = false;
        if routes[irq as usize].via_ioapic {
            (true, routes[irq as usize].gsi, true)
        } else {
            (false, 0, true)
        }
    });
    if unmask_hw {
        let _ = ioapic::unmask_gsi(gsi);
    } else if was_masked {
        klog_info!("IRQ: Cannot unmask line (no IOAPIC route configured)");
    }
}

fn log_unhandled_irq(irq: u8, vector: u8) {
    if irq as usize >= IRQ_LINES {
        klog_info!("IRQ: Spurious vector {} received", vector);
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
    klog_info!(
        "IRQ: Unhandled IRQ {} (vector {}) - masking line",
        irq,
        vector
    );
}

extern "C" fn timer_irq_handler(irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    (irq, _frame, _ctx);
    unsafe {
        TIMER_TICK_COUNTER = TIMER_TICK_COUNTER.wrapping_add(1);
        let tick = TIMER_TICK_COUNTER;
        if tick <= 3 {
            klog_debug!("IRQ: Timer tick #{}", tick);
        }
        sched_bridge::timer_tick();
    }
}

extern "C" fn keyboard_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
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

extern "C" fn mouse_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {
    unsafe {
        let status = io::inb(PS2_STATUS_PORT);
        if status & 0x20 == 0 {
            // Bit 5 must be set for mouse data
            return;
        }

        let data = io::inb(PS2_DATA_PORT);
        mouse::mouse_handle_irq(data);
    }
}

fn irq_program_ioapic_route(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }

    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        sched_bridge::kernel_panic(
            b"IRQ: APIC/IOAPIC unavailable during route programming\0".as_ptr() as *const c_char,
        );
    }

    let mut gsi = 0u32;
    let mut legacy_flags = 0u32;
    if ioapic::legacy_irq_info(irq, &mut gsi, &mut legacy_flags) != 0 {
        sched_bridge::kernel_panic(
            b"IRQ: Failed to translate legacy IRQ\0".as_ptr() as *const c_char
        );
    }

    let vector = IRQ_BASE_VECTOR.wrapping_add(irq) as u8;
    let lapic_id = apic::get_id() as u8;
    let flags =
        IOAPIC_FLAG_DELIVERY_FIXED | IOAPIC_FLAG_DEST_PHYSICAL | legacy_flags | IOAPIC_FLAG_MASK;

    if ioapic::config_irq(gsi, vector, lapic_id, flags) != 0 {
        sched_bridge::kernel_panic(
            b"IRQ: Failed to program IOAPIC route\0".as_ptr() as *const c_char
        );
    }

    let masked = with_irq_tables(|table, routes| {
        routes[irq as usize].via_ioapic = true;
        routes[irq as usize].gsi = gsi;
        table[irq as usize].masked
    });

    let polarity = if legacy_flags & IOAPIC_FLAG_POLARITY_LOW != 0 {
        "active-low"
    } else {
        "active-high"
    };
    let trigger = if legacy_flags & IOAPIC_FLAG_TRIGGER_LEVEL != 0 {
        "level"
    } else {
        "edge"
    };

    klog_info!(
        "IRQ: IOAPIC route IRQ {} -> GSI {}, vector 0x{:x} ({}, {})",
        irq,
        gsi,
        vector,
        polarity,
        trigger
    );

    if masked {
        let _ = ioapic::mask_gsi(gsi);
    } else {
        let _ = ioapic::unmask_gsi(gsi);
    }
}

fn irq_setup_ioapic_routes() {
    if !apic::is_enabled() || ioapic::is_ready() == 0 {
        sched_bridge::kernel_panic(
            b"IRQ: APIC/IOAPIC not ready during dispatcher init\0".as_ptr() as *const c_char,
        );
    }

    irq_program_ioapic_route(LEGACY_IRQ_TIMER);
    irq_program_ioapic_route(LEGACY_IRQ_KEYBOARD);
    irq_program_ioapic_route(LEGACY_IRQ_MOUSE);
    irq_program_ioapic_route(LEGACY_IRQ_COM1);
}

pub fn get_timer_ticks() -> u64 {
    unsafe { TIMER_TICK_COUNTER }
}

pub fn init() {
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
    mouse::mouse_init();

    let _ = register_handler(
        LEGACY_IRQ_TIMER,
        Some(timer_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = register_handler(
        LEGACY_IRQ_KEYBOARD,
        Some(keyboard_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );
    let _ = register_handler(
        LEGACY_IRQ_MOUSE,
        Some(mouse_irq_handler),
        core::ptr::null_mut(),
        core::ptr::null(),
    );

    // Enable interrupts globally once IDT/APIC/IOAPIC routes and handlers are ready.
    cpu::enable_interrupts();
}

pub fn register_handler(
    irq: u8,
    handler: Option<IrqHandler>,
    context: *mut c_void,
    name: *const c_char,
) -> i32 {
    if irq as usize >= IRQ_LINES {
        klog_info!("IRQ: Attempted to register handler for invalid line");
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

    if !name.is_null() {
        let name_str = unsafe { CStr::from_ptr(name) }
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_debug!("IRQ: Registered handler for line {} ({})", irq, name_str);
    } else {
        klog_debug!("IRQ: Registered handler for line {}", irq);
    }

    unmask_irq_line(irq);
    wl_currency::award_win();
    0
}

pub fn unregister_handler(irq: u8) {
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
    klog_debug!("IRQ: Unregistered handler for line {}", irq);
}

pub fn enable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    with_irq_tables(|table, _| {
        table[irq as usize].reported_unhandled = false;
    });
    unmask_irq_line(irq);
}

pub fn disable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    mask_irq_line(irq);
}
pub fn irq_dispatch(frame: *mut InterruptFrame) {
    if frame.is_null() {
        klog_info!("IRQ: Received null frame");
        return;
    }

    let frame_ref = unsafe { &mut *frame };
    let vector = (frame_ref.vector & 0xFF) as u8;
    let expected_cs = frame_ref.cs;
    let expected_rip = frame_ref.rip;

    if !IRQ_SYSTEM_INITIALIZED.load(Ordering::Relaxed) {
        klog_info!("IRQ: Dispatch received before initialization");
        if vector >= IRQ_BASE_VECTOR {
            acknowledge_irq();
        }
        return;
    }

    if vector < IRQ_BASE_VECTOR {
        klog_info!("IRQ: Received non-IRQ vector {}", vector);
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
        klog_info!("IRQ: Frame corruption detected on IRQ {} - aborting", irq);
        kdiag_dump_interrupt_frame(frame);
        sched_bridge::kernel_panic(b"IRQ: frame corrupted\0".as_ptr() as *const c_char);
    }

    acknowledge_irq();
    sched_bridge::handle_post_irq();
}

#[repr(C)]
pub struct IrqStats {
    count: u64,
    last_timestamp: u64,
}

pub fn get_stats(irq: u8, out_stats: *mut IrqStats) -> i32 {
    if irq as usize >= IRQ_LINES || out_stats.is_null() {
        return -1;
    }
    with_irq_tables(|table, _| unsafe {
        (*out_stats).count = table[irq as usize].count;
        (*out_stats).last_timestamp = table[irq as usize].last_timestamp;
    });
    0
}
