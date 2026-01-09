use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_lib::{klog_debug, klog_info};

use crate::gdt::gdt_set_ist;
use crate::idt::{
    idt_set_ist, EXCEPTION_DOUBLE_FAULT, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_PAGE_FAULT,
    EXCEPTION_STACK_FAULT,
};
use crate::kernel_panic::kernel_panic;

// Import exception stack constants from mm
use slopos_mm::mm_constants::{
    EXCEPTION_STACK_GUARD_SIZE, EXCEPTION_STACK_PAGES, EXCEPTION_STACK_REGION_BASE,
    EXCEPTION_STACK_REGION_STRIDE, EXCEPTION_STACK_SIZE, PAGE_KERNEL_RW, PAGE_SIZE_4KB,
};

#[repr(C)]
pub struct ExceptionStackInfoConfig {
    name: &'static [u8],
    vector: u8,
    ist_index: u8,
    region_base: u64,
    guard_start: u64,
    guard_end: u64,
    stack_base: u64,
    stack_top: u64,
    stack_size: u64,
}

impl ExceptionStackInfoConfig {
    const fn new(index: usize, name: &'static [u8], vector: u8, ist_index: u8) -> Self {
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
            region_base,
            guard_start,
            guard_end,
            stack_base,
            stack_top,
            stack_size: EXCEPTION_STACK_SIZE,
        }
    }
}

struct ExceptionStackMetrics {
    peak_usage: AtomicU64,
    out_of_bounds_reported: AtomicBool,
}

impl ExceptionStackMetrics {
    const fn new() -> Self {
        Self {
            peak_usage: AtomicU64::new(0),
            out_of_bounds_reported: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.peak_usage.store(0, Ordering::Relaxed);
        self.out_of_bounds_reported.store(false, Ordering::Relaxed);
    }

    fn mark_out_of_bounds_once(&self) -> bool {
        self.out_of_bounds_reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

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
}

static STACK_CONFIGS: [ExceptionStackInfoConfig; 4] = [
    ExceptionStackInfoConfig::new(0, b"Double Fault\0", EXCEPTION_DOUBLE_FAULT, 1),
    ExceptionStackInfoConfig::new(1, b"Stack Fault\0", EXCEPTION_STACK_FAULT, 2),
    ExceptionStackInfoConfig::new(2, b"General Protection\0", EXCEPTION_GENERAL_PROTECTION, 3),
    ExceptionStackInfoConfig::new(3, b"Page Fault\0", EXCEPTION_PAGE_FAULT, 4),
];

static STACK_METRICS: [ExceptionStackMetrics; 4] = [
    ExceptionStackMetrics::new(),
    ExceptionStackMetrics::new(),
    ExceptionStackMetrics::new(),
    ExceptionStackMetrics::new(),
];

use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::page_alloc::alloc_page_frame;
use slopos_mm::paging::map_page_4kb;
use slopos_abi::addr::VirtAddr;
use core::ptr;

fn bytes_to_str(bytes: &[u8]) -> &str {
    CStr::from_bytes_with_nul(bytes)
        .ok()
        .and_then(|c| c.to_str().ok())
        .unwrap_or("<invalid>")
}

fn find_stack_index_by_vector(vector: u8) -> Option<usize> {
    STACK_CONFIGS.iter().position(|cfg| cfg.vector == vector)
}

fn find_stack_index_by_address(addr: u64) -> Option<usize> {
    STACK_CONFIGS
        .iter()
        .position(|cfg| addr >= cfg.guard_start && addr < cfg.stack_top)
}

fn map_stack_pages(stack: &ExceptionStackInfoConfig) {
    for page in 0..EXCEPTION_STACK_PAGES {
        let virt_addr = stack.stack_base + page * PAGE_SIZE_4KB;
        let phys_addr = alloc_page_frame(0);
        if phys_addr.is_null() {
            kernel_panic(
                b"safe_stack_init: Failed to allocate exception stack page\0".as_ptr()
                    as *const c_char,
            );
        }
        let Some(virt) = phys_addr.to_virt_checked() else {
            kernel_panic(
                b"safe_stack_init: HHDM unavailable for exception stack page\0".as_ptr()
                    as *const c_char,
            );
        };
        unsafe {
            ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, PAGE_SIZE_4KB as usize);
        }
        if map_page_4kb(VirtAddr::new(virt_addr), phys_addr, PAGE_KERNEL_RW) != 0 {
            kernel_panic(
                b"safe_stack_init: Failed to map exception stack page\0".as_ptr() as *const c_char,
            );
        }
    }
}
pub fn safe_stack_init() {
    klog_debug!("SAFE STACK: Initializing dedicated IST stacks");

    for (i, stack) in STACK_CONFIGS.iter().enumerate() {
        STACK_METRICS[i].reset();

        map_stack_pages(stack);

        gdt_set_ist(stack.ist_index, stack.stack_top);
        idt_set_ist(stack.vector, stack.ist_index);

        klog_debug!(
            "SAFE STACK: Vector {} uses IST{} @ 0x{:x} - 0x{:x}",
            stack.vector,
            stack.ist_index,
            stack.stack_base,
            stack.stack_top
        );
    }

    klog_debug!("SAFE STACK: IST stacks ready");
}
pub fn safe_stack_record_usage(vector: u8, frame_ptr: u64) {
    let Some(idx) = find_stack_index_by_vector(vector) else {
        return;
    };
    let stack = &STACK_CONFIGS[idx];
    let metrics = &STACK_METRICS[idx];

    if frame_ptr < stack.stack_base || frame_ptr > stack.stack_top {
        if metrics.mark_out_of_bounds_once() {
            klog_info!(
                "SAFE STACK WARNING: RSP outside managed stack for vector {}",
                vector
            );
        }
        return;
    }

    let usage = stack.stack_top - frame_ptr;
    if metrics.record_usage(usage) {
        klog_debug!(
            "SAFE STACK: New peak usage on {} stack: {} bytes",
            bytes_to_str(stack.name),
            usage
        );

        if usage > stack.stack_size - PAGE_SIZE_4KB {
            klog_info!(
                "SAFE STACK WARNING: {} stack within one page of guard",
                bytes_to_str(stack.name)
            );
        }
    }
}
pub fn safe_stack_guard_fault(fault_addr: u64, stack_name: *mut *const c_char) -> i32 {
    if let Some(idx) = find_stack_index_by_address(fault_addr) {
        let stack = &STACK_CONFIGS[idx];
        if fault_addr >= stack.guard_start && fault_addr < stack.guard_end {
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
