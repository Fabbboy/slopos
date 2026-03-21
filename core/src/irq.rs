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
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::arch::idt::{
    IRQ_BASE_VECTOR, MSI_VECTOR_BASE, MSI_VECTOR_COUNT, MSI_VECTOR_END, SYSCALL_VECTOR,
};
use slopos_arch::{InterruptFrame, tsc};
pub use slopos_kernel_services::driver_runtime::IRQ_LINES;
use slopos_sync::InitFlag;
use slopos_sync::IrqMutex;
use slopos_utils::string::cstr_to_str;
use slopos_utils::{kdiag_dump_interrupt_frame, klog_debug, klog_info};

use crate::scheduler::scheduler::{TrapExitSource, scheduler_handoff_on_trap_exit};
use slopos_kernel_services::platform;

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
static IRQ_SYSTEM_INIT: InitFlag = InitFlag::new();
/// Global timer tick counter. Incremented atomically by the timer IRQ handler.
/// Uses Relaxed ordering since we only need eventual consistency for statistics.
static TIMER_TICK_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Global keyboard event counter. Incremented atomically by the keyboard IRQ handler.
/// Uses Relaxed ordering since we only need eventual consistency for statistics.
static KEYBOARD_EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);
static IRQ_TABLE_LOCK: IrqMutex<()> = IrqMutex::new(());

/// Access IRQ tables under lock.
#[inline]
fn with_irq_tables<R>(
    f: impl FnOnce(&mut [IrqEntry; IRQ_LINES], &mut [IrqRouteState; IRQ_LINES]) -> R,
) -> R {
    let _guard = IRQ_TABLE_LOCK.lock();
    unsafe {
        f(
            &mut *IRQ_TABLES.entries_mut(),
            &mut *IRQ_TABLES.routes_mut(),
        )
    }
}

/// Send EOI to acknowledge interrupt.
#[inline]
fn acknowledge_irq() {
    platform::irq_send_eoi();
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
        platform::irq_mask_gsi(gsi);
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
        platform::irq_unmask_gsi(gsi);
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

#[inline]
pub fn get_timer_ticks() -> u64 {
    TIMER_TICK_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_timer_ticks() {
    TIMER_TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn get_keyboard_event_counter() -> u64 {
    KEYBOARD_EVENT_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_keyboard_events() {
    KEYBOARD_EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
}

/// Initialize the IRQ framework (call early, before handler registration).
pub fn init() {
    with_irq_tables(|table, routes| {
        for i in 0..IRQ_LINES {
            table[i] = IrqEntry::new();
            routes[i] = IrqRouteState::new();
        }
    });
    TIMER_TICK_COUNTER.store(0, Ordering::Relaxed);
    KEYBOARD_EVENT_COUNTER.store(0, Ordering::Relaxed);
    IRQ_SYSTEM_INIT.mark_set();
    klog_debug!("IRQ: Framework initialized");
}

/// Check if IRQ system is initialized.
pub fn is_initialized() -> bool {
    IRQ_SYSTEM_INIT.is_set_relaxed()
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

    if !IRQ_SYSTEM_INIT.is_set_relaxed() {
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
        // Check if this is an MSI vector before rejecting.
        if vector >= MSI_VECTOR_BASE && vector < MSI_VECTOR_END {
            msi_dispatch_inner(vector, frame);
            acknowledge_irq();
            scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
            return;
        }
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
        panic!("IRQ: frame corrupted");
    }

    acknowledge_irq();
    scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
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

// =============================================================================
// MSI (Message Signaled Interrupts) Vector Allocator & Dispatch
// =============================================================================
//
// MSI bypasses the IOAPIC — devices write directly to the LAPIC.
// Vectors 48–223 (MSI_VECTOR_BASE..MSI_VECTOR_END) are reserved for MSI.
// The allocator is a simple atomic bitmap; the handler table is lock-protected.

/// MSI handler function signature.
///
/// `vector`: the IDT vector that fired (48–223).
/// `frame`:  pointer to the saved interrupt frame.
/// `ctx`:    opaque context pointer supplied at registration.
pub type MsiHandler = extern "C" fn(vector: u8, frame: *mut InterruptFrame, ctx: *mut c_void);

/// Per-vector MSI registration entry.
#[derive(Clone, Copy)]
struct MsiEntry {
    handler: Option<MsiHandler>,
    context: *mut c_void,
    /// BDF identifier for diagnostics (bus << 16 | dev << 8 | func).
    _device_bdf: u32,
    count: u64,
}

impl MsiEntry {
    const fn empty() -> Self {
        Self {
            handler: None,
            context: core::ptr::null_mut(),
            _device_bdf: 0,
            count: 0,
        }
    }
}

/// Bitmap words covering MSI_VECTOR_COUNT bits.
/// 176 vectors → 3 × u64 = 192 bits (top 16 bits unused).
const MSI_BITMAP_WORDS: usize = (MSI_VECTOR_COUNT + 63) / 64;

static MSI_BITMAP: [AtomicU64; MSI_BITMAP_WORDS] = {
    // const-initialise each word to 0 (all free).
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MSI_BITMAP_WORDS]
};

/// MSI handler table container.
struct MsiTables {
    entries: UnsafeCell<[MsiEntry; MSI_VECTOR_COUNT]>,
}

// SAFETY: access is serialised by MSI_TABLE_LOCK.
unsafe impl Sync for MsiTables {}

impl MsiTables {
    const fn new() -> Self {
        const EMPTY: MsiEntry = MsiEntry::empty();
        Self {
            entries: UnsafeCell::new([EMPTY; MSI_VECTOR_COUNT]),
        }
    }
}

static MSI_TABLE: MsiTables = MsiTables::new();
static MSI_TABLE_LOCK: IrqMutex<()> = IrqMutex::new(());

/// Access the MSI handler table under lock.
#[inline]
fn with_msi_table<R>(f: impl FnOnce(&mut [MsiEntry; MSI_VECTOR_COUNT]) -> R) -> R {
    let _guard = MSI_TABLE_LOCK.lock();
    unsafe { f(&mut *MSI_TABLE.entries.get()) }
}

// ---------------------------------------------------------------------------
// Vector allocator
// ---------------------------------------------------------------------------

/// Allocate a single MSI vector.
///
/// Returns the IDT vector number (48–223) or `None` if exhausted.
pub fn msi_alloc_vector() -> Option<u8> {
    for word_idx in 0..MSI_BITMAP_WORDS {
        loop {
            let current = MSI_BITMAP[word_idx].load(Ordering::Relaxed);
            if current == u64::MAX {
                break; // this word is full
            }
            let bit = (!current).trailing_zeros() as usize;
            let abs_bit = word_idx * 64 + bit;
            if abs_bit >= MSI_VECTOR_COUNT {
                return None; // past the valid range
            }
            let vector = MSI_VECTOR_BASE + abs_bit as u8;
            let new = current | (1u64 << bit);
            if vector == SYSCALL_VECTOR {
                // SYSCALL_VECTOR (0x80) lives inside the MSI range but is
                // reserved for the INT 0x80 trap gate.  Mark the bit as used
                // so future scans skip it, and continue searching.
                let _ = MSI_BITMAP[word_idx].compare_exchange_weak(
                    current,
                    new,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                break; // move to next word (or retry if CAS failed)
            }
            if MSI_BITMAP[word_idx]
                .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(vector);
            }
            // CAS raced — retry this word.
        }
    }
    None
}

/// Free a previously-allocated MSI vector.
///
/// Also clears the handler table entry so stale interrupts are safely ignored.
pub fn msi_free_vector(vector: u8) {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_END {
        return;
    }
    let idx = (vector - MSI_VECTOR_BASE) as usize;
    let word_idx = idx / 64;
    let bit = idx % 64;
    MSI_BITMAP[word_idx].fetch_and(!(1u64 << bit), Ordering::Release);
    with_msi_table(|table| {
        table[idx] = MsiEntry::empty();
    });
}

// ---------------------------------------------------------------------------
// Handler registration
// ---------------------------------------------------------------------------

/// Register an interrupt handler for an allocated MSI vector.
///
/// `vector` must have been returned by [`msi_alloc_vector`].
/// `device_bdf` is `(bus << 16) | (dev << 8) | func` for debug logging.
pub fn msi_register_handler(
    vector: u8,
    handler: MsiHandler,
    context: *mut c_void,
    device_bdf: u32,
) -> i32 {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_END {
        klog_info!(
            "MSI: register_handler: vector 0x{:02x} out of range",
            vector
        );
        return -1;
    }
    let idx = (vector - MSI_VECTOR_BASE) as usize;
    with_msi_table(|table| {
        table[idx] = MsiEntry {
            handler: Some(handler),
            context,
            _device_bdf: device_bdf,
            count: 0,
        };
    });
    klog_debug!(
        "MSI: Registered handler for vector 0x{:02x} (BDF 0x{:06x})",
        vector,
        device_bdf
    );
    0
}

/// Unregister the handler for an MSI vector.
pub fn msi_unregister_handler(vector: u8) {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_END {
        return;
    }
    let idx = (vector - MSI_VECTOR_BASE) as usize;
    with_msi_table(|table| {
        table[idx] = MsiEntry::empty();
    });
    klog_debug!("MSI: Unregistered handler for vector 0x{:02x}", vector);
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch an MSI interrupt.  Called from [`irq_dispatch`] for vectors in
/// the MSI range.  EOI is sent by the caller.
fn msi_dispatch_inner(vector: u8, frame: *mut InterruptFrame) {
    let idx = (vector - MSI_VECTOR_BASE) as usize;

    let snapshot = with_msi_table(|table| {
        let entry = &mut table[idx];
        match entry.handler {
            Some(h) => {
                entry.count = entry.count.wrapping_add(1);
                Some((h, entry.context))
            }
            None => None,
        }
    });

    if let Some((handler, context)) = snapshot {
        handler(vector, frame, context);
    } else {
        // No handler registered — log once, then ignore.
        klog_debug!("MSI: Unhandled vector 0x{:02x}", vector);
    }
}

// ---------------------------------------------------------------------------
// Query helpers (used by drivers)
// ---------------------------------------------------------------------------

/// Return the number of currently-allocated MSI vectors.
pub fn msi_allocated_count() -> usize {
    let mut count = 0usize;
    for word_idx in 0..MSI_BITMAP_WORDS {
        count += MSI_BITMAP[word_idx].load(Ordering::Relaxed).count_ones() as usize;
    }
    count
}

/// Check whether a specific vector is allocated.
pub fn msi_vector_is_allocated(vector: u8) -> bool {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_END {
        return false;
    }
    let idx = (vector - MSI_VECTOR_BASE) as usize;
    let word_idx = idx / 64;
    let bit = idx % 64;
    (MSI_BITMAP[word_idx].load(Ordering::Relaxed) & (1u64 << bit)) != 0
}
