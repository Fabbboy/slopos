//! Per-CPU Interrupt Stack Table (IST) stacks: the TSS's 7 slots give the
//! critical exception vectors and the two high-rate IRQs a known-good stack
//! regardless of what the interrupted context had done to its own.
//!
//! Every region is 64 KiB apart and carries an unmapped guard page at its base,
//! so an overflow lands in the page-fault classifier rather than in the
//! neighbouring stack.

use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::arch::idt::{
    EXCEPTION_DOUBLE_FAULT, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_PAGE_FAULT,
    EXCEPTION_STACK_FAULT, IRQ_BASE_VECTOR,
};
use slopos_arch::{MAX_CPUS, get_current_cpu};
use slopos_mm::kernel_mappings::kernel_map_4kb_frame;
use slopos_mm::kernel_meta::KernelMeta;
use slopos_mm::memory_layout_defs::{
    EMERGENCY_DSTACK_GUARD_SIZE, EMERGENCY_DSTACK_PAGES, EMERGENCY_DSTACK_REGION_BASE,
    EMERGENCY_DSTACK_REGION_STRIDE, EMERGENCY_SAFE_STACK_GUARD_SIZE, EMERGENCY_SAFE_STACK_PAGES,
    EMERGENCY_SAFE_STACK_REGION_BASE, EMERGENCY_SAFE_STACK_REGION_STRIDE, EXC_DSTACK_GUARD_SIZE,
    EXC_DSTACK_PAGES, EXC_DSTACK_REGION_BASE, EXC_DSTACK_REGION_STRIDE, EXCEPTION_STACK_GUARD_SIZE,
    EXCEPTION_STACK_PAGES, EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE,
    EXCEPTION_STACK_SIZE,
};
use slopos_mm::paging::{get_page_size, virt_to_phys};
use slopos_mm::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use slopos_ostd::mm::frame::Frame;
use slopos_ostd::{klog_debug, klog_info};

/// Purely a logging/diagnostics label; it selects no behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IstCategory {
    CriticalException = 0,
    StackException = 1,
    MemoryException = 2,
    HighFreqIrq = 3,
    Reserved = 4,
}

impl IstCategory {
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

#[repr(C)]
pub struct IstStackConfig {
    /// NUL-terminated.
    name: &'static [u8],
    vector: u8,
    /// TSS slot 1-7, as stored in the IDT entry.
    ist_index: u8,
    category: IstCategory,
    region_base: u64,
    guard_start: u64,
    guard_end: u64,
    stack_base: u64,
    stack_top: u64,
    stack_size: u64,
}

impl IstStackConfig {
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

    fn name_str(&self) -> &str {
        CStr::from_bytes_with_nul(self.name)
            .ok()
            .and_then(|c| c.to_str().ok())
            .unwrap_or("<invalid>")
    }
}

struct IstStackMetrics {
    /// Bytes below the stack top.
    peak_usage: AtomicU64,
    out_of_bounds_reported: AtomicBool,
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

    /// True only for the first report, so the log cannot be flooded.
    fn mark_out_of_bounds_once(&self) -> bool {
        self.out_of_bounds_reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    /// True if `usage` is a new peak.
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

    fn record_entry(&self) {
        self.entry_count.fetch_add(1, Ordering::Relaxed);
    }
}

const IRQ_KEYBOARD_VECTOR: u8 = IRQ_BASE_VECTOR + 1;

const IRQ_MOUSE_VECTOR: u8 = IRQ_BASE_VECTOR + 12;

pub(crate) const IST_STACK_COUNT: usize = 6;

/// Order matters: the index determines virtual address placement.
///
/// The NMI (vector 2) is deliberately absent and must stay absent: on x86-64
/// *any* `IRET` unblocks NMI, so an IST would let a nested NMI reset RSP to the
/// IST top and overwrite the frame the outer handler is still using.
static IST_CONFIGS: [IstStackConfig; IST_STACK_COUNT] = [
    IstStackConfig::new(
        0,
        b"Double Fault\0",
        EXCEPTION_DOUBLE_FAULT,
        1,
        IstCategory::CriticalException,
    ),
    IstStackConfig::new(
        1,
        b"Stack Fault\0",
        EXCEPTION_STACK_FAULT,
        2,
        IstCategory::StackException,
    ),
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

static IST_METRICS: [IstStackMetrics; IST_STACK_COUNT] = [
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
    IstStackMetrics::new(),
];

static CPU_IST_MAPPED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

fn find_index_by_vector(vector: u8) -> Option<usize> {
    IST_CONFIGS.iter().position(|cfg| cfg.vector == vector)
}

#[inline]
fn stack_region_base_for_cpu(cpu_id: usize, stack_idx: usize) -> u64 {
    EXCEPTION_STACK_REGION_BASE
        + ((cpu_id as u64 * IST_STACK_COUNT as u64) + stack_idx as u64)
            * EXCEPTION_STACK_REGION_STRIDE
}

/// `(guard_start, guard_end, stack_base, stack_top)` of CPU `cpu_id`'s IST
/// stack `stack_idx`. The guard page occupies `[guard_start, guard_end)`.
#[inline]
pub(crate) fn stack_bounds_for_cpu(cpu_id: usize, stack_idx: usize) -> (u64, u64, u64, u64) {
    let guard_start = stack_region_base_for_cpu(cpu_id, stack_idx);
    let guard_end = guard_start + EXCEPTION_STACK_GUARD_SIZE;
    let stack_base = guard_end;
    let stack_top = stack_base + EXCEPTION_STACK_SIZE;
    (guard_start, guard_end, stack_base, stack_top)
}

fn find_index_by_address(addr: u64) -> Option<(usize, usize)> {
    for cpu_id in 0..MAX_CPUS {
        if !CPU_IST_MAPPED[cpu_id].load(Ordering::Acquire) {
            continue;
        }
        for idx in 0..IST_STACK_COUNT {
            let (guard_start, _guard_end, _stack_base, stack_top) =
                stack_bounds_for_cpu(cpu_id, idx);
            if addr >= guard_start && addr < stack_top {
                return Some((cpu_id, idx));
            }
        }
    }
    None
}

/// Only the BSP may link these regions, and only from [`premap_cpus`] before
/// any AP is bootstrapped: 32 stack slots share one leaf page table, so two
/// CPUs mapping concurrently each install a leaf for the same page-directory
/// entry and the last install discards the other's stack pages. Checked in
/// release too — the resulting reset is indistinguishable from a hardware fault.
fn assert_bsp_is_mapping(region: &str) {
    let cpu_id = get_current_cpu();
    if cpu_id != 0 {
        // A panic message does not reach the console from an AP this early in
        // bring-up, but klog does; the assertion only has to stop the machine.
        klog_info!(
            "IST: CPU {} is linking page tables for {}; these regions share \
             page-directory entries across CPUs, so only the BSP may map them, \
             from premap_cpus, before any AP is bootstrapped",
            cpu_id,
            region
        );
    }
    assert!(
        cpu_id == 0,
        "ist_stacks: non-BSP CPU linked kernel page tables"
    );
}

fn map_stack_pages(stack: &IstStackConfig, stack_base: u64) {
    assert_bsp_is_mapping("an IST stack");
    for page in 0..EXCEPTION_STACK_PAGES {
        let virt_addr = stack_base + page * PAGE_SIZE_4KB;
        // The handle goes straight to the mapper: an IST stack lives as long as
        // the kernel, so the leaf entry holds the never-released reference
        // rather than `into_phys` leaving it held by nobody.
        let frame = Frame::<KernelMeta>::alloc_zeroed().unwrap_or_else(|| {
            panic!(
                "ist_stacks_init: Failed to allocate zeroed page for {} stack",
                stack.name_str()
            )
        });
        if kernel_map_4kb_frame(VirtAddr::new(virt_addr), frame, PageFlags::KERNEL_RW.bits()) != 0 {
            let vaddr = VirtAddr::new(virt_addr);
            let mapped_phys = virt_to_phys(vaddr);
            let page_size = get_page_size(vaddr);
            klog_info!(
                "IST: map failure {} vaddr=0x{:x} mapped_phys=0x{:x} page_size=0x{:x}",
                stack.name_str(),
                virt_addr,
                mapped_phys.as_u64(),
                page_size
            );
            panic!(
                "ist_stacks_init: Failed to map page for {} stack",
                stack.name_str()
            );
        }
    }
}

fn ensure_cpu_stacks_mapped(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    if CPU_IST_MAPPED[cpu_id].load(Ordering::Acquire) {
        return;
    }

    for (idx, stack) in IST_CONFIGS.iter().enumerate() {
        let (_guard_start, _guard_end, stack_base, _stack_top) = stack_bounds_for_cpu(cpu_id, idx);
        map_stack_pages(stack, stack_base);
    }

    // The SafeStack DATA stack instrumented handler code reaches through
    // `gs:[ist_unsafe_sp]`.
    map_exc_dstack_pages(cpu_id);

    map_emergency_stacks_pages(cpu_id);

    CPU_IST_MAPPED[cpu_id].store(true, Ordering::Release);
}

/// `(guard_start, usable_base, top)` of CPU `cpu_id`'s exception data stack.
/// The guard page occupies `[guard_start, usable_base)`.
#[inline]
pub(crate) fn exc_dstack_bounds_for_cpu(cpu_id: usize) -> (u64, u64, u64) {
    let region_base = EXC_DSTACK_REGION_BASE + cpu_id as u64 * EXC_DSTACK_REGION_STRIDE;
    let usable_base = region_base + EXC_DSTACK_GUARD_SIZE;
    let top = region_base + EXC_DSTACK_REGION_STRIDE;
    (region_base, usable_base, top)
}

/// Leaves the guard page at the region base unmapped.
fn map_exc_dstack_pages(cpu_id: usize) {
    assert_bsp_is_mapping("an exception data stack");
    let (_guard_start, usable_base, _top) = exc_dstack_bounds_for_cpu(cpu_id);
    for page in 0..EXC_DSTACK_PAGES {
        let virt_addr = usable_base + page * PAGE_SIZE_4KB;
        let frame = Frame::<KernelMeta>::alloc_zeroed().unwrap_or_else(|| {
            panic!(
                "ist_stacks: failed to allocate exception data-stack page for CPU {}",
                cpu_id
            )
        });
        if kernel_map_4kb_frame(VirtAddr::new(virt_addr), frame, PageFlags::KERNEL_RW.bits()) != 0 {
            panic!(
                "ist_stacks: failed to map exception data-stack page for CPU {}",
                cpu_id
            );
        }
    }
}

/// Must run after [`ensure_cpu_stacks_mapped`] and before any IDT IST selector
/// is installed, since an exception on an IST stack selects this data stack.
fn prime_exc_dstack(cpu_id: usize) {
    let (_guard_start, _usable_base, top) = exc_dstack_bounds_for_cpu(cpu_id);
    slopos_arch::pcr::set_local_ist_unsafe_sp(top);
}

/// Used by `retire_faulted_cpu` to re-prime `ist_unsafe_sp` after a fatal user
/// fault abandons the IST data stack without unwinding.
pub fn exc_dstack_top_current_cpu() -> u64 {
    let (_guard_start, _usable_base, top) = exc_dstack_bounds_for_cpu(get_current_cpu());
    top
}

/// `(guard_start, usable_base, top)`; exposed for the SafeStack regression tests.
pub fn exc_dstack_bounds_current_cpu() -> (u64, u64, u64) {
    exc_dstack_bounds_for_cpu(get_current_cpu())
}

/// Returns the offending CPU id on a data-stack guard-page hit, so the #PF
/// handler reports an overflow instead of recursing.
pub fn exc_dstack_guard_fault(fault_addr: u64) -> Option<usize> {
    for cpu_id in 0..MAX_CPUS {
        if !CPU_IST_MAPPED[cpu_id].load(Ordering::Acquire) {
            continue;
        }
        let (guard_start, usable_base, _top) = exc_dstack_bounds_for_cpu(cpu_id);
        if fault_addr >= guard_start && fault_addr < usable_base {
            return Some(cpu_id);
        }
    }
    None
}

/// `(guard_start, usable_base, top)` of CPU `cpu_id`'s emergency data stack.
#[inline]
pub(crate) fn emergency_dstack_bounds_for_cpu(cpu_id: usize) -> (u64, u64, u64) {
    let region_base = EMERGENCY_DSTACK_REGION_BASE + cpu_id as u64 * EMERGENCY_DSTACK_REGION_STRIDE;
    let usable_base = region_base + EMERGENCY_DSTACK_GUARD_SIZE;
    let top = region_base + EMERGENCY_DSTACK_REGION_STRIDE;
    (region_base, usable_base, top)
}

/// `(guard_start, usable_base, top)` of CPU `cpu_id`'s emergency safe stack.
#[inline]
pub(crate) fn emergency_safe_bounds_for_cpu(cpu_id: usize) -> (u64, u64, u64) {
    let region_base =
        EMERGENCY_SAFE_STACK_REGION_BASE + cpu_id as u64 * EMERGENCY_SAFE_STACK_REGION_STRIDE;
    let usable_base = region_base + EMERGENCY_SAFE_STACK_GUARD_SIZE;
    let top = region_base + EMERGENCY_SAFE_STACK_REGION_STRIDE;
    (region_base, usable_base, top)
}

fn map_one_stack_region(usable_base: u64, pages: u64, what: &str, cpu_id: usize) {
    assert_bsp_is_mapping(what);
    for page in 0..pages {
        let virt_addr = usable_base + page * PAGE_SIZE_4KB;
        let frame = Frame::<KernelMeta>::alloc_zeroed().unwrap_or_else(|| {
            panic!(
                "ist_stacks: failed to alloc {} page for CPU {}",
                what, cpu_id
            )
        });
        if kernel_map_4kb_frame(VirtAddr::new(virt_addr), frame, PageFlags::KERNEL_RW.bits()) != 0 {
            panic!("ist_stacks: failed to map {} page for CPU {}", what, cpu_id);
        }
    }
}

/// Each guard page is left unmapped: overflow → guard #PF → `panic_abort_raw`.
fn map_emergency_stacks_pages(cpu_id: usize) {
    let (_g, safe_base, _t) = emergency_safe_bounds_for_cpu(cpu_id);
    map_one_stack_region(
        safe_base,
        EMERGENCY_SAFE_STACK_PAGES,
        "emergency safe-stack",
        cpu_id,
    );
    let (_g, data_base, _t) = emergency_dstack_bounds_for_cpu(cpu_id);
    map_one_stack_region(
        data_base,
        EMERGENCY_DSTACK_PAGES,
        "emergency data-stack",
        cpu_id,
    );
}

/// Must run after [`ensure_cpu_stacks_mapped`] and before the IDT is live, for
/// the same reason as [`prime_exc_dstack`].
fn prime_emergency_stacks(cpu_id: usize) {
    let (_g, _u, safe_top) = emergency_safe_bounds_for_cpu(cpu_id);
    let (_g, _u, data_top) = emergency_dstack_bounds_for_cpu(cpu_id);
    slopos_arch::pcr::set_local_panic_safe_sp(safe_top);
    slopos_arch::pcr::set_local_panic_unsafe_sp(data_top);
}

/// `(guard_start, usable_base, top)`; exposed for the Reliable Abort Core tests.
pub fn emergency_safe_bounds_current_cpu() -> (u64, u64, u64) {
    emergency_safe_bounds_for_cpu(get_current_cpu())
}

/// `(guard_start, usable_base, top)` of the current CPU's emergency data stack.
pub fn emergency_dstack_bounds_current_cpu() -> (u64, u64, u64) {
    emergency_dstack_bounds_for_cpu(get_current_cpu())
}

/// A hit means the fatal-fault report itself overflowed — the #PF handler must
/// degrade to `panic_abort_raw`.
pub fn emergency_stack_guard_fault(fault_addr: u64) -> Option<usize> {
    for cpu_id in 0..MAX_CPUS {
        if !CPU_IST_MAPPED[cpu_id].load(Ordering::Acquire) {
            continue;
        }
        let (sg, su, _st) = emergency_safe_bounds_for_cpu(cpu_id);
        if fault_addr >= sg && fault_addr < su {
            return Some(cpu_id);
        }
        let (dg, du, _dt) = emergency_dstack_bounds_for_cpu(cpu_id);
        if fault_addr >= dg && fault_addr < du {
            return Some(cpu_id);
        }
    }
    None
}

/// Initializes all IST stacks.
///
/// # Panics
/// Panics if memory allocation or mapping fails.
///
/// # Safety
/// Must be called after:
/// - Memory subsystem is initialized (page allocator, paging)
/// - GDT/TSS is initialized
/// - IDT is initialized (but before interrupts are enabled)
pub fn ist_stacks_init<'b>(ctx: &mut slopos_hermetic::BootCtx<'b, slopos_hermetic::BspInit>) {
    klog_debug!(
        "IST: Initializing {} dedicated interrupt stacks",
        IST_STACK_COUNT
    );

    for (i, _stack) in IST_CONFIGS.iter().enumerate() {
        IST_METRICS[i].reset();
    }

    ensure_cpu_stacks_mapped(0);
    ist_bind_current_cpu(ctx);

    klog_info!(
        "IST: Initialized {} stacks ({} exceptions, {} IRQs)",
        IST_STACK_COUNT,
        IST_CONFIGS
            .iter()
            .filter(|c| c.category != IstCategory::HighFreqIrq)
            .count(),
        IST_CONFIGS
            .iter()
            .filter(|c| c.category == IstCategory::HighFreqIrq)
            .count()
    );
}

/// Maps the IST, exception and emergency stacks of CPUs `0..cpu_count`.
///
/// The single writer required by [`assert_bsp_is_mapping`]: it runs on the BSP
/// before the first AP is bootstrapped. Every CPU still calls
/// [`ensure_cpu_stacks_mapped`] later and finds the work already done.
pub fn premap_cpus(cpu_count: usize) {
    for cpu_id in 0..cpu_count.min(MAX_CPUS) {
        ensure_cpu_stacks_mapped(cpu_id);
    }
}

/// Bind preallocated IST stacks into the current CPU's TSS/IDT context; must
/// run on every CPU after its per-CPU GDT/TSS is installed. `K: CpuInitKind`
/// keeps the surface callable from BSP-init, AP-init and test scopes.
pub fn ist_bind_current_cpu<'b, K: slopos_hermetic::CpuInitKind>(
    ctx: &mut slopos_hermetic::BootCtx<'b, K>,
) {
    let cpu_id = get_current_cpu();
    ensure_cpu_stacks_mapped(cpu_id);

    // Both must precede the IST selectors installed below: once an IST is live,
    // an exception on it makes `__safestack_pointer_address` select
    // `ist_unsafe_sp`, which must already point at a mapped top.
    prime_exc_dstack(cpu_id);
    prime_emergency_stacks(cpu_id);

    for (idx, stack) in IST_CONFIGS.iter().enumerate() {
        let (_guard_start, _guard_end, stack_base, stack_top) = stack_bounds_for_cpu(cpu_id, idx);
        let Some(slot) = slopos_arch::arch::gdt::IstSlot::from_index(stack.ist_index) else {
            klog_info!(
                "IST: invalid ist_index {} for vector {}",
                stack.ist_index,
                stack.vector
            );
            continue;
        };
        // The `'static` lifetime holds because these pages live for the kernel
        // image.
        let stack_top_typed = slopos_hermetic::KernelStackTop::from_kernel_va(stack_top);

        crate::gdt::gdt_set_ist(ctx, slot, stack_top_typed);
        crate::idt::idt_set_ist(ctx, stack.vector, slot);

        klog_debug!(
            "IST: CPU{} {} [{}] vec={} IST{} @ 0x{:x}-0x{:x}",
            cpu_id,
            stack.name_str(),
            stack.category.name(),
            stack.vector,
            stack.ist_index,
            stack_base,
            stack_top
        );
    }
}

/// Called from the common exception handler; `frame_ptr` is the interrupt
/// frame's address, i.e. RSP on entry. Vectors without an IST stack are ignored.
pub fn ist_record_usage(vector: u8, frame_ptr: u64) {
    let Some(idx) = find_index_by_vector(vector) else {
        return;
    };

    let stack = &IST_CONFIGS[idx];
    let metrics = &IST_METRICS[idx];
    let cpu_id = get_current_cpu();
    let (_guard_start, _guard_end, stack_base, stack_top) = stack_bounds_for_cpu(cpu_id, idx);

    metrics.record_entry();

    if frame_ptr < stack_base || frame_ptr > stack_top {
        if metrics.mark_out_of_bounds_once() {
            klog_info!(
                "IST WARNING: CPU{} RSP 0x{:x} outside {} stack bounds (0x{:x}-0x{:x})",
                cpu_id,
                frame_ptr,
                stack.name_str(),
                stack_base,
                stack_top
            );
        }
        return;
    }

    let usage = stack_top - frame_ptr;

    metrics.record_usage(usage);
}

/// Classifies a CR2 value: `Some` is a guard-page hit, so the #PF handler can
/// name the overflowed stack instead of reporting a generic fault. The slice is
/// the stack's NUL-terminated static name.
pub fn ist_guard_fault(fault_addr: u64) -> Option<&'static [u8]> {
    let (cpu_id, idx) = find_index_by_address(fault_addr)?;
    let stack = &IST_CONFIGS[idx];
    let (guard_start, guard_end, _stack_base, _stack_top) = stack_bounds_for_cpu(cpu_id, idx);

    if fault_addr >= guard_start && fault_addr < guard_end {
        Some(stack.name)
    } else {
        None
    }
}

pub fn ist_is_on_ist_stack(rsp: u64) -> bool {
    for cpu_id in 0..MAX_CPUS {
        if !CPU_IST_MAPPED[cpu_id].load(Ordering::Acquire) {
            continue;
        }
        for idx in 0..IST_STACK_COUNT {
            let (_guard_start, _guard_end, stack_base, stack_top) =
                stack_bounds_for_cpu(cpu_id, idx);
            if rsp >= stack_base && rsp <= stack_top {
                return true;
            }
        }
    }
    false
}

/// `(peak_usage, entry_count)`, or `None` if the vector has no IST stack.
pub fn ist_get_stats(vector: u8) -> Option<(u64, u64)> {
    let idx = find_index_by_vector(vector)?;
    let metrics = &IST_METRICS[idx];
    Some((
        metrics.peak_usage.load(Ordering::Relaxed),
        metrics.entry_count.load(Ordering::Relaxed),
    ))
}

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
