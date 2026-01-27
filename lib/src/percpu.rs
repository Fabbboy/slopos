//! Per-CPU Data Infrastructure for SMP Support
//!
//! This module provides the foundational per-CPU data structure and access
//! mechanisms required for symmetric multiprocessing (SMP) support.
//!
//! # Architecture
//!
//! Each CPU has its own `PerCpuData` instance, accessed via the LAPIC ID
//! mapping. Future optimization can use GS-base addressing for faster access.
//!
//! # Usage
//!
//! ```ignore
//! let cpu_id = get_current_cpu();
//! let data = get_percpu_data();
//! ```

use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::InitFlag;

const IA32_GS_BASE: u32 = 0xC000_0101;

/// Maximum number of CPUs supported.
/// Must match MAX_CPUS in mm/src/tlb.rs and mm/src/page_alloc.rs.
pub const MAX_CPUS: usize = 256;

/// Magic value indicating per-CPU data is initialized.
const PERCPU_INIT_MAGIC: u64 = 0x5350_4350_5543_5055; // "SPCPCPU\0"

/// Per-CPU data structure - one instance per CPU.
///
/// This structure is cache-line aligned to prevent false sharing between CPUs.
/// Critical fields are placed at the beginning for fast GS-based access if
/// we migrate to that approach later.
#[repr(C, align(64))]
pub struct PerCpuData {
    pub cpu_id: u32,
    pub apic_id: u32,
    pub init_magic: u64,
    pub current_task: AtomicPtr<()>,
    pub kernel_stack_top: AtomicU64,
    pub preempt_count: AtomicU32,
    pub in_interrupt: AtomicBool,
    pub scheduler: AtomicPtr<()>,
    pub online: AtomicBool,
    pub context_switches: AtomicU64,
    pub interrupt_count: AtomicU64,
    pub syscall_pid: AtomicU32,
}

impl PerCpuData {
    /// Create a new uninitialized per-CPU data structure.
    pub const fn new() -> Self {
        Self {
            cpu_id: 0,
            apic_id: 0,
            init_magic: 0,
            current_task: AtomicPtr::new(ptr::null_mut()),
            kernel_stack_top: AtomicU64::new(0),
            preempt_count: AtomicU32::new(0),
            in_interrupt: AtomicBool::new(false),
            scheduler: AtomicPtr::new(ptr::null_mut()),
            online: AtomicBool::new(false),
            context_switches: AtomicU64::new(0),
            interrupt_count: AtomicU64::new(0),
            syscall_pid: AtomicU32::new(u32::MAX),
        }
    }

    /// Check if this per-CPU data has been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.init_magic == PERCPU_INIT_MAGIC
    }

    /// Mark this per-CPU data as initialized.
    pub fn mark_initialized(&mut self) {
        self.init_magic = PERCPU_INIT_MAGIC;
    }
}

// SAFETY: PerCpuData uses atomics for all mutable fields and is only
// accessed by the owning CPU (except during initialization).
unsafe impl Send for PerCpuData {}
unsafe impl Sync for PerCpuData {}

/// Global array of per-CPU data structures.
/// Index 0 is always the BSP.
static mut PER_CPU_DATA: [PerCpuData; MAX_CPUS] = {
    const INIT: PerCpuData = PerCpuData::new();
    [INIT; MAX_CPUS]
};

/// Mapping from APIC ID to CPU index.
/// INVALID_CPU_IDX means the APIC ID is not mapped.
const INVALID_CPU_IDX: u32 = u32::MAX;
static APIC_ID_TO_CPU_IDX: [AtomicU32; MAX_CPUS] = {
    const INIT: AtomicU32 = AtomicU32::new(INVALID_CPU_IDX);
    [INIT; MAX_CPUS]
};

/// Number of CPUs currently initialized.
static CPU_COUNT: AtomicU32 = AtomicU32::new(0);

/// BSP's APIC ID (set during init).
static BSP_APIC_ID: AtomicU32 = AtomicU32::new(0);

static PERCPU_INIT: InitFlag = InitFlag::new();

/// Initialize per-CPU data for a specific CPU.
///
/// # Arguments
/// * `cpu_id` - The CPU index (0 for BSP, 1+ for APs)
/// * `apic_id` - The LAPIC ID for this CPU
///
/// # Safety
/// Must be called exactly once per CPU during initialization.
pub fn init_percpu_for_cpu(cpu_id: usize, apic_id: u32) {
    if cpu_id >= MAX_CPUS {
        return;
    }

    // Register APIC ID -> CPU index mapping
    if (apic_id as usize) < MAX_CPUS {
        APIC_ID_TO_CPU_IDX[apic_id as usize].store(cpu_id as u32, Ordering::Release);
    }

    // Initialize per-CPU data
    // SAFETY: Each CPU initializes its own entry exactly once
    unsafe {
        let data = &mut PER_CPU_DATA[cpu_id];
        data.cpu_id = cpu_id as u32;
        data.apic_id = apic_id;
        data.current_task.store(ptr::null_mut(), Ordering::Release);
        data.kernel_stack_top.store(0, Ordering::Release);
        data.preempt_count.store(0, Ordering::Release);
        data.in_interrupt.store(false, Ordering::Release);
        data.scheduler.store(ptr::null_mut(), Ordering::Release);
        data.online.store(false, Ordering::Release);
        data.context_switches.store(0, Ordering::Release);
        data.interrupt_count.store(0, Ordering::Release);
        data.mark_initialized();
    }

    let current_count = CPU_COUNT.load(Ordering::Acquire);
    if cpu_id as u32 >= current_count {
        CPU_COUNT.store(cpu_id as u32 + 1, Ordering::Release);
    }
}

#[inline(always)]
fn write_gs_base(value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") IA32_GS_BASE,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags)
        );
    }
}

pub fn activate_gs_base_for_cpu(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        let data = &PER_CPU_DATA[cpu_id];
        let addr = data as *const PerCpuData as u64;
        write_gs_base(addr);

        // Verify GS-based read works immediately after setting
        let test_cpu_id: u32;
        asm!(
            "mov {:e}, gs:[0]",
            out(reg) test_cpu_id,
            options(nostack, preserves_flags, readonly)
        );
        crate::klog_info!(
            "PERCPU: CPU {} GS_BASE=0x{:016x} verify_read={}",
            cpu_id,
            addr,
            test_cpu_id
        );
    }
}

pub fn init_bsp(apic_id: u32) {
    if !PERCPU_INIT.init_once() {
        return;
    }
    BSP_APIC_ID.store(apic_id, Ordering::Release);
    init_percpu_for_cpu(0, apic_id);
    activate_gs_base_for_cpu(0);
}

#[inline]
pub fn get_current_cpu() -> usize {
    if !PERCPU_INIT.is_set() {
        return 0;
    }

    let apic_id = read_lapic_id();
    cpu_index_from_apic_id(apic_id).unwrap_or(0)
}

/// Convert APIC ID to CPU index.
#[inline]
pub fn cpu_index_from_apic_id(apic_id: u32) -> Option<usize> {
    if (apic_id as usize) >= MAX_CPUS {
        return None;
    }
    let idx = APIC_ID_TO_CPU_IDX[apic_id as usize].load(Ordering::Acquire);
    if idx == INVALID_CPU_IDX {
        None
    } else {
        Some(idx as usize)
    }
}

/// Convert CPU index to APIC ID.
#[inline]
pub fn apic_id_from_cpu_index(cpu_id: usize) -> Option<u32> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    // SAFETY: cpu_id is bounds checked
    let data = unsafe { &PER_CPU_DATA[cpu_id] };
    if data.is_initialized() {
        Some(data.apic_id)
    } else {
        None
    }
}

/// Get the current CPU's per-CPU data.
///
/// # Returns
/// A reference to the current CPU's PerCpuData, or BSP's data if not initialized.
#[inline]
pub fn get_percpu_data() -> &'static PerCpuData {
    let cpu_id = get_current_cpu();
    // SAFETY: cpu_id is always < MAX_CPUS
    unsafe { &PER_CPU_DATA[cpu_id] }
}

/// Get the per-CPU data for a specific CPU.
///
/// # Arguments
/// * `cpu_id` - The CPU index
///
/// # Returns
/// A reference to the specified CPU's PerCpuData, or None if invalid.
#[inline]
pub fn get_percpu_data_for(cpu_id: usize) -> Option<&'static PerCpuData> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    // SAFETY: cpu_id is bounds checked
    let data = unsafe { &PER_CPU_DATA[cpu_id] };
    if data.is_initialized() {
        Some(data)
    } else {
        None
    }
}

/// Get mutable per-CPU data for a specific CPU.
///
/// # Safety
/// Caller must ensure exclusive access (typically the owning CPU).
#[inline]
pub unsafe fn get_percpu_data_for_mut(cpu_id: usize) -> Option<&'static mut PerCpuData> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    // SAFETY: cpu_id is bounds checked, caller ensures exclusive access
    let data = unsafe { &mut PER_CPU_DATA[cpu_id] };
    if data.is_initialized() {
        Some(data)
    } else {
        None
    }
}

/// Get the number of initialized CPUs.
#[inline]
pub fn get_cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire) as usize
}

/// Get the number of online (running scheduler) CPUs.
#[inline]
pub fn get_online_cpu_count() -> usize {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    let mut online = 0;
    for i in 0..count.min(MAX_CPUS) {
        // SAFETY: i is bounds checked
        if unsafe { PER_CPU_DATA[i].online.load(Ordering::Relaxed) } {
            online += 1;
        }
    }
    online
}

/// Mark a CPU as online (ready to run tasks).
pub fn mark_cpu_online(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    // SAFETY: cpu_id is bounds checked
    unsafe {
        PER_CPU_DATA[cpu_id].online.store(true, Ordering::Release);
    }
}

/// Mark a CPU as offline.
pub fn mark_cpu_offline(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    // SAFETY: cpu_id is bounds checked
    unsafe {
        PER_CPU_DATA[cpu_id].online.store(false, Ordering::Release);
    }
}

/// Check if a CPU is online.
#[inline]
pub fn is_cpu_online(cpu_id: usize) -> bool {
    if cpu_id >= MAX_CPUS {
        return false;
    }
    // SAFETY: cpu_id is bounds checked
    unsafe { PER_CPU_DATA[cpu_id].online.load(Ordering::Acquire) }
}

/// Get the BSP's APIC ID.
#[inline]
pub fn get_bsp_apic_id() -> u32 {
    BSP_APIC_ID.load(Ordering::Acquire)
}

/// Check if the current CPU is the BSP.
#[inline]
pub fn is_bsp() -> bool {
    get_current_cpu() == 0
}

/// Callback function to read LAPIC ID from the APIC driver.
static LAPIC_ID_FN: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// Register the LAPIC ID reader function from the APIC driver.
pub fn register_lapic_id_fn(f: fn() -> u32) {
    LAPIC_ID_FN.store(f as *mut (), Ordering::Release);
}

fn read_lapic_id() -> u32 {
    let fn_ptr = LAPIC_ID_FN.load(Ordering::Acquire);
    if !fn_ptr.is_null() {
        // SAFETY: Function pointer was set by register_lapic_id_fn with a valid fn() -> u32
        let f: fn() -> u32 = unsafe { core::mem::transmute(fn_ptr) };
        return f();
    }

    // Fallback: Read initial APIC ID from CPUID leaf 1
    // This works even before the APIC driver is initialized
    let (_, ebx, _, _) = crate::cpu::cpuid(1);
    ((ebx >> 24) & 0xFF) as u32
}

/// Set the current task pointer for this CPU.
#[inline]
pub fn set_current_task(task: *mut ()) {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe {
            PER_CPU_DATA[cpu_id]
                .current_task
                .store(task, Ordering::Release);
        }
    }
}

/// Get the current task pointer for this CPU.
#[inline]
pub fn get_current_task() -> *mut () {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe { PER_CPU_DATA[cpu_id].current_task.load(Ordering::Acquire) }
    } else {
        ptr::null_mut()
    }
}

/// Set the kernel stack top for this CPU.
#[inline]
pub fn set_kernel_stack_top(stack_top: u64) {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe {
            PER_CPU_DATA[cpu_id]
                .kernel_stack_top
                .store(stack_top, Ordering::Release);
        }
    }
}

/// Get the kernel stack top for this CPU.
#[inline]
pub fn get_kernel_stack_top() -> u64 {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe {
            PER_CPU_DATA[cpu_id]
                .kernel_stack_top
                .load(Ordering::Acquire)
        }
    } else {
        0
    }
}

/// Increment the context switch counter for this CPU.
#[inline]
pub fn increment_context_switches() {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe {
            PER_CPU_DATA[cpu_id]
                .context_switches
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Increment the interrupt counter for this CPU.
#[inline]
pub fn increment_interrupt_count() {
    let cpu_id = get_current_cpu();
    if cpu_id < MAX_CPUS {
        // SAFETY: cpu_id is bounds checked
        unsafe {
            PER_CPU_DATA[cpu_id]
                .interrupt_count
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub type SendIpiToCpuFn = fn(u32, u8);

static SEND_IPI_TO_CPU_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

pub fn register_send_ipi_to_cpu_fn(f: SendIpiToCpuFn) {
    SEND_IPI_TO_CPU_FN.store(f as *mut (), Ordering::Release);
}

pub fn send_ipi_to_cpu(target_apic_id: u32, vector: u8) {
    let fn_ptr = SEND_IPI_TO_CPU_FN.load(Ordering::Acquire);
    if !fn_ptr.is_null() {
        let f: SendIpiToCpuFn = unsafe { core::mem::transmute(fn_ptr) };
        f(target_apic_id, vector);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percpu_data_size() {
        // Ensure cache line alignment
        assert_eq!(core::mem::align_of::<PerCpuData>(), 64);
    }
}
