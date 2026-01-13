//! Interrupt Stack Table (IST) Management for SlopOS
//!
//! This module manages dedicated stacks for interrupt and exception handling using
//! the x86-64 IST (Interrupt Stack Table) mechanism. IST allows specific interrupt
//! vectors to automatically switch to a pre-configured stack, preventing stack
//! overflow when interrupts occur on an already-stressed stack.
//!
//! # IST Slot Allocation Strategy
//!
//! The TSS provides 7 IST slots (IST1-IST7). We allocate them by priority:
//!
//! | IST | Category            | Vector(s) | Rationale                                    |
//! |-----|---------------------|-----------|----------------------------------------------|
//! | 1   | Critical Exception  | 8 (DF)    | Double fault MUST have its own stack         |
//! | 2   | Stack Exception     | 12 (SF)   | Stack fault cannot use current stack         |
//! | 3   | Memory Exception    | 13 (GP)   | GP faults need isolation for debugging       |
//! | 4   | Memory Exception    | 14 (PF)   | Page faults need clean stack for handlers    |
//! | 5   | High-Freq IRQ       | 33 (KB)   | Keyboard IRQ can rapid-fire, causes nesting  |
//! | 6   | High-Freq IRQ       | 44 (MS)   | Mouse IRQ same rapid-fire potential          |
//! | 7   | Reserved            | -         | Future use (NMI, timer, etc.)                |
//!
//! # Why Hardware IRQs Need IST
//!
//! When a hardware IRQ fires, the CPU pushes an interrupt frame (~40 bytes) onto
//! the current stack. If the IRQ handler calls functions, more stack is used.
//! With rapid input (e.g., holding multiple keys), IRQs can queue up faster than
//! they're processed, causing:
//!
//! 1. Deep call stacks as handlers call scheduler, input routing, etc.
//! 2. If interrupts nest (shouldn't with proper EOI, but edge cases exist),
//!    each nested IRQ consumes more stack
//! 3. Eventually the kernel stack overflows into adjacent memory regions
//!
//! By assigning high-frequency IRQs to dedicated IST stacks, each IRQ handler
//! gets a fresh, known-good stack regardless of what was happening before.
//!
//! # Memory Layout
//!
//! Each IST stack has a guard page (unmapped) below it to catch overflows:
//!
//! ```text
//! +------------------+ <- stack_top (IST entry points here)
//! |                  |
//! |   Usable Stack   |  32 KB (8 pages)
//! |                  |
//! +------------------+ <- stack_base
//! |   Guard Page     |  4 KB (unmapped, triggers page fault on overflow)
//! +------------------+ <- guard_start = region_base
//! ```
//!
//! Stacks are spaced 64 KB apart in virtual address space.

use core::ffi::{CStr, c_char};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_abi::arch::x86_64::idt::{
    EXCEPTION_DOUBLE_FAULT, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_PAGE_FAULT,
    EXCEPTION_STACK_FAULT, IRQ_BASE_VECTOR,
};
use slopos_lib::{klog_debug, klog_info};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mm_constants::{
    EXCEPTION_STACK_GUARD_SIZE, EXCEPTION_STACK_PAGES, EXCEPTION_STACK_REGION_BASE,
    EXCEPTION_STACK_REGION_STRIDE, EXCEPTION_STACK_SIZE, PAGE_SIZE_4KB, PageFlags,
};
use slopos_mm::page_alloc::alloc_page_frame;
use slopos_mm::paging::map_page_4kb;

use crate::gdt::gdt_set_ist;
use crate::idt::idt_set_ist;

// =============================================================================
// IST Categories
// =============================================================================

/// Category of interrupt/exception for logging and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IstCategory {
    /// Critical exceptions that must never share a stack (Double Fault)
    CriticalException = 0,
    /// Stack-related exceptions (Stack Fault)
    StackException = 1,
    /// Memory protection exceptions (GP, PF)
    MemoryException = 2,
    /// High-frequency hardware IRQs (Keyboard, Mouse)
    HighFreqIrq = 3,
    /// Reserved for future use
    Reserved = 4,
}

impl IstCategory {
    /// Returns a human-readable name for the category.
    pub const fn name(&self) -> &'static str {
        match self {
            IstCategory::CriticalException => "Critical",
            IstCategory::StackException => "Stack",
            IstCategory::MemoryException => "Memory",
            IstCategory::HighFreqIrq => "IRQ",
            IstCategory::Reserved => "Reserved",
        }
    }
}

// =============================================================================
// IST Stack Configuration
// =============================================================================

/// Configuration for a single IST stack entry.
#[repr(C)]
pub struct IstStackConfig {
    /// Human-readable name (null-terminated bytes)
    name: &'static [u8],
    /// Interrupt/exception vector number
    vector: u8,
    /// IST index (1-7, as stored in IDT entry)
    ist_index: u8,
    /// Category for logging/diagnostics
    category: IstCategory,
    /// Base of the entire region (guard page start)
    region_base: u64,
    /// Start of guard page
    guard_start: u64,
    /// End of guard page (= start of usable stack)
    guard_end: u64,
    /// Base of usable stack
    stack_base: u64,
    /// Top of usable stack (IST entry points here)
    stack_top: u64,
    /// Size of usable stack in bytes
    stack_size: u64,
}

impl IstStackConfig {
    /// Creates a new IST stack configuration.
    ///
    /// # Arguments
    /// * `index` - Position in the stack array (0-based), determines virtual address
    /// * `name` - Human-readable name (must be null-terminated)
    /// * `vector` - Interrupt vector number
    /// * `ist_index` - IST slot (1-7)
    /// * `category` - Category for logging
    const fn new(
        index: usize,
        name: &'static [u8],
        vector: u8,
        ist_index: u8,
        category: IstCategory,
    ) -> Self {
        let region_base =
            EXCEPTION_STACK_REGION_BASE + index as u64 * EXCEPTION_STACK_REGION_STRIDE;
        let guard_start = region_base;
        let guard_end = guard_start + EXCEPTION_STACK_GUARD_SIZE;
        let stack_base = guard_end;
        let stack_top = stack_base + EXCEPTION_STACK_SIZE;

        Self {
            name,
            vector,
            ist_index,
            category,
            region_base,
            guard_start,
            guard_end,
            stack_base,
            stack_top,
            stack_size: EXCEPTION_STACK_SIZE,
        }
    }

    /// Returns the name as a string slice (for logging).
    fn name_str(&self) -> &str {
        CStr::from_bytes_with_nul(self.name)
            .ok()
            .and_then(|c| c.to_str().ok())
            .unwrap_or("<invalid>")
    }
}

// =============================================================================
// IST Stack Metrics
// =============================================================================

/// Runtime metrics for a single IST stack.
struct IstStackMetrics {
    /// Peak stack usage observed (bytes from top)
    peak_usage: AtomicU64,
    /// Whether we've reported an out-of-bounds RSP (report once to avoid spam)
    out_of_bounds_reported: AtomicBool,
    /// Number of times this stack was entered
    entry_count: AtomicU64,
}

impl IstStackMetrics {
    const fn new() -> Self {
        Self {
            peak_usage: AtomicU64::new(0),
            out_of_bounds_reported: AtomicBool::new(false),
            entry_count: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.peak_usage.store(0, Ordering::Relaxed);
        self.out_of_bounds_reported.store(false, Ordering::Relaxed);
        self.entry_count.store(0, Ordering::Relaxed);
    }

    /// Marks out-of-bounds as reported. Returns true if this was the first report.
    fn mark_out_of_bounds_once(&self) -> bool {
        self.out_of_bounds_reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    /// Records stack usage. Returns true if this is a new peak.
    fn record_usage(&self, usage: u64) -> bool {
        let mut current = self.peak_usage.load(Ordering::Relaxed);
        while usage > current {
            match self.peak_usage.compare_exchange_weak(
                current,
                usage,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(prev) => current = prev,
            }
        }
        false
    }

    /// Increments entry count.
    fn record_entry(&self) {
        self.entry_count.fetch_add(1, Ordering::Relaxed);
    }
}

// =============================================================================
// Static Configuration
// =============================================================================

/// IRQ vector for keyboard (IRQ1 remapped to vector 33).
const IRQ_KEYBOARD_VECTOR: u8 = IRQ_BASE_VECTOR + 1;

/// IRQ vector for mouse (IRQ12 remapped to vector 44).
const IRQ_MOUSE_VECTOR: u8 = IRQ_BASE_VECTOR + 12;

/// Total number of IST stacks we configure.
const IST_STACK_COUNT: usize = 6;

/// Static configuration for all IST stacks.
///
/// Order matters: index determines virtual address placement.
static IST_CONFIGS: [IstStackConfig; IST_STACK_COUNT] = [
    // Critical exceptions - must have dedicated stacks
    IstStackConfig::new(
        0,
        b"Double Fault\0",
        EXCEPTION_DOUBLE_FAULT,
        1,
        IstCategory::CriticalException,
    ),
    // Stack exceptions - cannot use current stack
    IstStackConfig::new(
        1,
        b"Stack Fault\0",
        EXCEPTION_STACK_FAULT,
        2,
        IstCategory::StackException,
    ),
    // Memory exceptions - need isolation for debugging
    IstStackConfig::new(
        2,
        b"General Protection\0",
        EXCEPTION_GENERAL_PROTECTION,
        3,
        IstCategory::MemoryException,
    ),
    IstStackConfig::new(
        3,
        b"Page Fault\0",
        EXCEPTION_PAGE_FAULT,
        4,
        IstCategory::MemoryException,
    ),
    // High-frequency IRQs - prevent stack overflow from rapid input
    IstStackConfig::new(
        4,
        b"Keyboard IRQ\0",
        IRQ_KEYBOARD_VECTOR,
        5,
        IstCategory::HighFreqIrq,
    ),
    IstStackConfig::new(
        5,
        b"Mouse IRQ\0",
        IRQ_MOUSE_VECTOR,
        6,
        IstCategory::HighFreqIrq,
    ),
];

/// Runtime metrics for each IST stack.
static IST_METRICS: [IstStackMetrics; IST_STACK_COUNT] = [
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
];

// =============================================================================
// Private Helpers
// =============================================================================

/// Finds the IST stack index by interrupt vector.
fn find_index_by_vector(vector: u8) -> Option<usize> {
    IST_CONFIGS.iter().position(|cfg| cfg.vector == vector)
}

/// Finds the IST stack index by fault address (for guard page detection).
fn find_index_by_address(addr: u64) -> Option<usize> {
    IST_CONFIGS
        .iter()
        .position(|cfg| addr >= cfg.guard_start && addr < cfg.stack_top)
}

/// Maps physical pages for an IST stack.
fn map_stack_pages(stack: &IstStackConfig) {
    for page in 0..EXCEPTION_STACK_PAGES {
        let virt_addr = stack.stack_base + page * PAGE_SIZE_4KB;
        let phys_addr = alloc_page_frame(0);
        if phys_addr.is_null() {
            panic!(
                "ist_stacks_init: Failed to allocate page for {} stack",
                stack.name_str()
            );
        }
        let Some(virt) = phys_addr.to_virt_checked() else {
            panic!(
                "ist_stacks_init: HHDM unavailable for {} stack page",
                stack.name_str()
            );
        };
        // Zero-initialize the stack page
        unsafe {
            ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, PAGE_SIZE_4KB as usize);
        }
        if map_page_4kb(
            VirtAddr::new(virt_addr),
            phys_addr,
            PageFlags::KERNEL_RW.bits(),
        ) != 0
        {
            panic!(
                "ist_stacks_init: Failed to map page for {} stack",
                stack.name_str()
            );
        }
    }
}

// =============================================================================
// Public API
// =============================================================================

/// Initializes all IST stacks.
///
/// This function:
/// 1. Allocates and maps physical pages for each IST stack
/// 2. Registers the stack tops in the TSS via GDT
/// 3. Configures the IDT entries to use the appropriate IST
///
/// # Panics
/// Panics if memory allocation or mapping fails.
///
/// # Safety
/// Must be called after:
/// - Memory subsystem is initialized (page allocator, paging)
/// - GDT/TSS is initialized
/// - IDT is initialized (but before interrupts are enabled)
pub fn ist_stacks_init() {
    klog_debug!("IST: Initializing {} dedicated interrupt stacks", IST_STACK_COUNT);

    for (i, stack) in IST_CONFIGS.iter().enumerate() {
        // Reset metrics for this stack
        IST_METRICS[i].reset();

        // Allocate and map the stack pages
        map_stack_pages(stack);

        // Register stack top in TSS
        gdt_set_ist(stack.ist_index, stack.stack_top);

        // Configure IDT to use this IST for the vector
        idt_set_ist(stack.vector, stack.ist_index);

        klog_debug!(
            "IST: {} [{}] vec={} IST{} @ 0x{:x}-0x{:x}",
            stack.name_str(),
            stack.category.name(),
            stack.vector,
            stack.ist_index,
            stack.stack_base,
            stack.stack_top
        );
    }

    klog_info!(
        "IST: Initialized {} stacks ({} exceptions, {} IRQs)",
        IST_STACK_COUNT,
        IST_CONFIGS.iter().filter(|c| c.category != IstCategory::HighFreqIrq).count(),
        IST_CONFIGS.iter().filter(|c| c.category == IstCategory::HighFreqIrq).count()
    );
}

/// Records stack usage for a given interrupt vector.
///
/// Called from the common exception handler to track stack usage and detect
/// potential overflow conditions. Only tracks vectors that have IST stacks.
///
/// # Arguments
/// * `vector` - The interrupt vector number
/// * `frame_ptr` - The current stack pointer (RSP from interrupt frame)
pub fn ist_record_usage(vector: u8, frame_ptr: u64) {
    let Some(idx) = find_index_by_vector(vector) else {
        // Not an IST-managed vector, nothing to track
        return;
    };

    let stack = &IST_CONFIGS[idx];
    let metrics = &IST_METRICS[idx];

    // Record that this stack was entered
    metrics.record_entry();

    // Check if RSP is within expected bounds
    if frame_ptr < stack.stack_base || frame_ptr > stack.stack_top {
        // RSP is outside our managed stack - might be using kernel stack
        // or something is very wrong. Report once to avoid log spam.
        if metrics.mark_out_of_bounds_once() {
            klog_info!(
                "IST WARNING: RSP 0x{:x} outside {} stack bounds (0x{:x}-0x{:x})",
                frame_ptr,
                stack.name_str(),
                stack.stack_base,
                stack.stack_top
            );
        }
        return;
    }

    // Calculate usage (stack grows down, so top - current = used)
    let usage = stack.stack_top - frame_ptr;

    // Record if this is a new peak
    if metrics.record_usage(usage) {
        // Use integer math to avoid FPU state corruption in interrupt context
        // pct_tenths = (usage * 1000) / stack_size gives percentage * 10
        let pct_tenths = if stack.stack_size > 0 {
            (usage * 1000 / stack.stack_size) as u32
        } else {
            0
        };
        klog_info!(
            "IST: New peak usage on {} stack: {} bytes ({}.{}%)",
            stack.name_str(),
            usage,
            pct_tenths / 10,
            pct_tenths % 10
        );

        // Warn if we're getting close to the guard page
        if usage > stack.stack_size - PAGE_SIZE_4KB {
            klog_info!(
                "IST WARNING: {} stack within one page of overflow ({} bytes used of {})",
                stack.name_str(),
                usage,
                stack.stack_size
            );
        }
    }
}

/// Checks if a fault address is within an IST guard page.
///
/// Called from the page fault handler to detect stack overflow conditions.
/// If the fault is in a guard page, we know it's a stack overflow and can
/// provide a meaningful error message instead of a generic page fault.
///
/// # Arguments
/// * `fault_addr` - The address that caused the page fault (from CR2)
/// * `stack_name` - Output: pointer to receive the stack name (if guard hit)
///
/// # Returns
/// * `1` if the fault address is in an IST guard page (stack overflow detected)
/// * `0` if the fault address is not in any IST guard page
pub fn ist_guard_fault(fault_addr: u64, stack_name: *mut *const c_char) -> i32 {
    if let Some(idx) = find_index_by_address(fault_addr) {
        let stack = &IST_CONFIGS[idx];

        // Check if the address is specifically in the guard page region
        if fault_addr >= stack.guard_start && fault_addr < stack.guard_end {
            // This is a guard page hit - stack overflow detected!
            if !stack_name.is_null() {
                unsafe {
                    *stack_name = stack.name.as_ptr() as *const c_char;
                }
            }
            return 1;
        }
    }
    0
}

/// Returns statistics for an IST stack by vector number.
///
/// # Arguments
/// * `vector` - The interrupt vector number
///
/// # Returns
/// * `Some((peak_usage, entry_count))` if vector has an IST stack
/// * `None` if vector doesn't have an IST stack
pub fn ist_get_stats(vector: u8) -> Option<(u64, u64)> {
    let idx = find_index_by_vector(vector)?;
    let metrics = &IST_METRICS[idx];
    Some((
        metrics.peak_usage.load(Ordering::Relaxed),
        metrics.entry_count.load(Ordering::Relaxed),
    ))
}

/// Dumps IST stack statistics to the kernel log.
///
/// Useful for debugging and monitoring stack usage.
pub fn ist_dump_stats() {
    klog_info!("=== IST Stack Statistics ===");
    for (i, stack) in IST_CONFIGS.iter().enumerate() {
        let metrics = &IST_METRICS[i];
        let peak = metrics.peak_usage.load(Ordering::Relaxed);
        let entries = metrics.entry_count.load(Ordering::Relaxed);
        let pct_tenths = if stack.stack_size > 0 {
            (peak * 1000 / stack.stack_size) as u32
        } else {
            0
        };

        klog_info!(
            "  {}: {} entries, peak {} bytes ({}.{}%)",
            stack.name_str(),
            entries,
            peak,
            pct_tenths / 10,
            pct_tenths % 10
        );
    }
    klog_info!("============================");
}

// =============================================================================
// Legacy API Compatibility
// =============================================================================

// These functions maintain backward compatibility with code that uses the old
// safe_stack naming. They simply delegate to the new functions.

/// Legacy alias for `ist_stacks_init`.
#[inline]
pub fn safe_stack_init() {
    ist_stacks_init()
}

/// Legacy alias for `ist_record_usage`.
#[inline]
pub fn safe_stack_record_usage(vector: u8, frame_ptr: u64) {
    ist_record_usage(vector, frame_ptr)
}

/// Legacy alias for `ist_guard_fault`.
#[inline]
pub fn safe_stack_guard_fault(fault_addr: u64, stack_name: *mut *const c_char) -> i32 {
    ist_guard_fault(fault_addr, stack_name)
}
