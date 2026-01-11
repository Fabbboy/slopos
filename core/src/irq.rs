//! IRQ dispatch framework for SlopOS.
//!
//! This module provides the IRQ table, dispatch logic, and handler registration API.
//! Hardware-specific handlers live in `drivers`, but the framework lives here in `core`
//! to maintain the one-way dependency: drivers -> core.
//!
//! Platform-specific operations (EOI, masking) are called via the platform service
//! function pointers registered at boot time.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::arch::IRQ_BASE_VECTOR;
use slopos_lib::spinlock::Spinlock;
use slopos_lib::string::cstr_to_str;
use slopos_lib::{InterruptFrame, kdiag_dump_interrupt_frame, klog_debug, klog_info, tsc};

use crate::platform;
use crate::scheduler::scheduler::scheduler_handle_post_irq;

/// Maximum number of IRQ lines supported.
pub const IRQ_LINES: usize = 16;

/// Legacy IRQ numbers.
pub const LEGACY_IRQ_TIMER: u8 = 0;
pub const LEGACY_IRQ_KEYBOARD: u8 = 1;
pub const LEGACY_IRQ_COM1: u8 = 4;
pub const LEGACY_IRQ_MOUSE: u8 = 12;

/// IRQ handler function signature.
pub type IrqHandler = extern "C" fn(u8, *mut InterruptFrame, *mut c_void);

/// Entry in the IRQ table.
#[derive(Clone, Copy)]
pub struct IrqEntry {
    handler: Option<IrqHandler>,
    context: *mut c_void,
    name: *const c_char,
    count: u64,
    last_timestamp: u64,
    masked: bool,
    reported_unhandled: bool,
}

impl IrqEntry {
    pub const fn new() -> Self {
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

/// IOAPIC route state for an IRQ line.
#[derive(Clone, Copy)]
pub struct IrqRouteState {
    pub via_ioapic: bool,
    pub gsi: u32,
}

impl IrqRouteState {
    pub const fn new() -> Self {
        Self {
            via_ioapic: false,
            gsi: 0,
        }
    }
}

/// IRQ tables container (entries + routes).
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

// Static state
static IRQ_TABLES: IrqTables = IrqTables::new();
static IRQ_SYSTEM_INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TIMER_TICK_COUNTER: u64 = 0;
static mut KEYBOARD_EVENT_COUNTER: u64 = 0;
static IRQ_TABLE_LOCK: Spinlock = Spinlock::new();

/// Access IRQ tables under lock.
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

/// Send EOI to acknowledge interrupt.
#[inline]
fn acknowledge_irq() {
    (platform::platform().irq_send_eoi)();
}

/// Mask an IRQ line.
pub fn mask_irq_line(irq: u8) {
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
        (platform::platform().irq_mask_gsi)(gsi);
    } else {
        klog_info!("IRQ: Mask request ignored for line (no IOAPIC route)");
    }
}

/// Unmask an IRQ line.
pub fn unmask_irq_line(irq: u8) {
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
        (platform::platform().irq_unmask_gsi)(gsi);
    } else if was_masked {
        klog_info!("IRQ: Cannot unmask line (no IOAPIC route configured)");
    }
}

/// Log an unhandled IRQ (only once per line).
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

/// Get timer tick counter.
pub fn get_timer_ticks() -> u64 {
    unsafe { TIMER_TICK_COUNTER }
}

/// Increment timer tick counter.
pub fn increment_timer_ticks() {
    unsafe {
        TIMER_TICK_COUNTER = TIMER_TICK_COUNTER.wrapping_add(1);
    }
}

/// Get keyboard event counter.
pub fn get_keyboard_event_counter() -> u64 {
    unsafe { KEYBOARD_EVENT_COUNTER }
}

/// Increment keyboard event counter.
pub fn increment_keyboard_events() {
    unsafe {
        KEYBOARD_EVENT_COUNTER = KEYBOARD_EVENT_COUNTER.wrapping_add(1);
    }
}

/// Initialize the IRQ framework (call early, before handler registration).
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
    klog_debug!("IRQ: Framework initialized");
}

/// Check if IRQ system is initialized.
pub fn is_initialized() -> bool {
    IRQ_SYSTEM_INITIALIZED.load(Ordering::Relaxed)
}

/// Set the IOAPIC route state for an IRQ line (called by drivers during setup).
pub fn set_irq_route(irq: u8, gsi: u32) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    with_irq_tables(|_, routes| {
        routes[irq as usize].via_ioapic = true;
        routes[irq as usize].gsi = gsi;
    });
}

/// Get the IOAPIC route state for an IRQ line.
pub fn get_irq_route(irq: u8) -> Option<IrqRouteState> {
    if irq as usize >= IRQ_LINES {
        return None;
    }
    with_irq_tables(|_, routes| Some(routes[irq as usize]))
}

/// Check if an IRQ line is masked.
pub fn is_masked(irq: u8) -> bool {
    if irq as usize >= IRQ_LINES {
        return true;
    }
    with_irq_tables(|table, _| table[irq as usize].masked)
}

/// Register an IRQ handler.
pub fn register_handler(
    irq: u8,
    handler: Option<IrqHandler>,
    context: *mut c_void,
    name: *const c_char,
) -> i32 {
    if irq as usize >= IRQ_LINES {
        klog_info!("IRQ: Attempted to register handler for invalid line");
        crate::wl_currency::award_loss();
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
        klog_debug!("IRQ: Registered handler for line {} ({})", irq, unsafe {
            cstr_to_str(name)
        });
    } else {
        klog_debug!("IRQ: Registered handler for line {}", irq);
    }

    unmask_irq_line(irq);
    crate::wl_currency::award_win();
    0
}

/// Unregister an IRQ handler.
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

/// Enable an IRQ line.
pub fn enable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    with_irq_tables(|table, _| {
        table[irq as usize].reported_unhandled = false;
    });
    unmask_irq_line(irq);
}

/// Disable an IRQ line.
pub fn disable_line(irq: u8) {
    if irq as usize >= IRQ_LINES {
        return;
    }
    mask_irq_line(irq);
}

/// Main IRQ dispatch function - called from IDT handler.
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
        platform::kernel_panic(b"IRQ: frame corrupted\0".as_ptr() as *const c_char);
    }

    acknowledge_irq();
    scheduler_handle_post_irq();
}

/// IRQ statistics structure.
#[repr(C)]
pub struct IrqStats {
    pub count: u64,
    pub last_timestamp: u64,
}

/// Get IRQ statistics for a line.
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
