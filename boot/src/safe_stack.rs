use core::ffi::c_char;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_lib::{klog_printf, KlogLevel};

use crate::gdt::gdt_set_ist;
use crate::idt::idt_set_ist;
use crate::kernel_panic::kernel_panic;

const EXCEPTION_STACK_REGION_BASE: u64 = 0xFFFFFFFFB0000000;
const EXCEPTION_STACK_REGION_STRIDE: u64 = 0x00010000;
const EXCEPTION_STACK_GUARD_SIZE: u64 = 0x1000;
const EXCEPTION_STACK_PAGES: u32 = 8;
const PAGE_SIZE_4KB: u64 = 0x1000;
const EXCEPTION_STACK_SIZE: u64 = EXCEPTION_STACK_PAGES as u64 * PAGE_SIZE_4KB;
const PAGE_KERNEL_RW: u64 = 0x003;

const EXCEPTION_DOUBLE_FAULT: u8 = 8;
const EXCEPTION_STACK_FAULT: u8 = 12;
const EXCEPTION_GENERAL_PROTECTION: u8 = 13;
const EXCEPTION_PAGE_FAULT: u8 = 14;

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
        let region_base = EXCEPTION_STACK_REGION_BASE + index as u64 * EXCEPTION_STACK_REGION_STRIDE;
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

unsafe extern "C" {
    fn alloc_page_frame(flags: u32) -> u64;
    fn mm_zero_physical_page(phys: u64) -> i32;
    fn map_page_4kb(virt: u64, phys: u64, flags: u64) -> i32;
}

fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

fn log_debug(msg: &[u8]) {
    log(KlogLevel::Debug, msg);
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
        let virt_addr = stack.stack_base + page as u64 * PAGE_SIZE_4KB;
        let phys_addr = unsafe { alloc_page_frame(0) };
        if phys_addr == 0 {
            kernel_panic(
                b"safe_stack_init: Failed to allocate exception stack page\0".as_ptr()
                    as *const c_char,
            );
        }
        if unsafe { mm_zero_physical_page(phys_addr) } != 0 {
            kernel_panic(
                b"safe_stack_init: Failed to zero exception stack page\0".as_ptr()
                    as *const c_char,
            );
        }
        if unsafe { map_page_4kb(virt_addr, phys_addr, PAGE_KERNEL_RW) } != 0 {
            kernel_panic(
                b"safe_stack_init: Failed to map exception stack page\0".as_ptr()
                    as *const c_char,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn safe_stack_init() {
    log_debug(b"SAFE STACK: Initializing dedicated IST stacks\0");

    for (i, stack) in STACK_CONFIGS.iter().enumerate() {
        STACK_METRICS[i].reset();

        map_stack_pages(stack);

        gdt_set_ist(stack.ist_index, stack.stack_top);
        idt_set_ist(stack.vector, stack.ist_index);

        unsafe {
            klog_printf(
                KlogLevel::Debug,
                b"SAFE STACK: Vector %u uses IST%u @ 0x%llx - 0x%llx\n\0".as_ptr() as *const c_char,
                stack.vector as u32,
                stack.ist_index as u32,
                stack.stack_base,
                stack.stack_top,
            );
        }
    }

    log_debug(b"SAFE STACK: IST stacks ready\0");
}

#[unsafe(no_mangle)]
pub extern "C" fn safe_stack_record_usage(vector: u8, frame_ptr: u64) {
    let Some(idx) = find_stack_index_by_vector(vector) else {
        return;
    };
    let stack = &STACK_CONFIGS[idx];
    let metrics = &STACK_METRICS[idx];

    if frame_ptr < stack.stack_base || frame_ptr > stack.stack_top {
        if metrics.mark_out_of_bounds_once() {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"SAFE STACK WARNING: RSP outside managed stack for vector %u\n\0".as_ptr()
                        as *const c_char,
                    vector as u32,
                );
            }
        }
        return;
    }

    let usage = stack.stack_top - frame_ptr;
    if metrics.record_usage(usage) {
        unsafe {
            klog_printf(
                KlogLevel::Debug,
                b"SAFE STACK: New peak usage on %s stack: %llu bytes\n\0".as_ptr() as *const c_char,
                stack.name.as_ptr() as *const c_char,
                usage,
            );
        }

        if usage > stack.stack_size - PAGE_SIZE_4KB {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"SAFE STACK WARNING: %s stack within one page of guard\n\0".as_ptr()
                        as *const c_char,
                    stack.name.as_ptr() as *const c_char,
                );
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn safe_stack_guard_fault(fault_addr: u64, stack_name: *mut *const c_char) -> i32 {
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
