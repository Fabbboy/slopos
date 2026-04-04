//! Unified per-CPU data infrastructure for SMP support.
//!
//! This module provides:
//! - The `ProcessorControlRegion` (PCR): the single per-CPU data structure,
//!   accessed via GS_BASE.
//! - APIC ID ↔ CPU index mapping.
//! - CPU lifecycle management (online/offline, counting).
//! - Per-CPU data accessors (current task, preemption, statistics).
//! - IPI callback registration.
//!
//! # Assembly Offsets (CRITICAL)
//!
//! Fields at offsets 0-24 in `ProcessorControlRegion` are accessed by assembly
//! code via `gs:[offset]`. DO NOT CHANGE these field positions without updating:
//! - `boot/idt_handlers.s` (syscall_entry)
//! - `core/context_switch.s` (context_switch_user)

use core::cell::SyncUnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::arch::gdt::{GdtDescriptor, GdtLayout, SegmentSelector, Tss64};
use crate::cpu::msr::Msr;

use crate::InitFlag;

// ==================== CONSTANTS ====================

/// Maximum number of CPUs supported.
pub const MAX_CPUS: usize = 256;

/// Per-CPU kernel stack size (64 KiB).
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Maximum number of statically-allocated AP PCRs.
const MAX_STATIC_APS: usize = 16;

// ==================== PCR STRUCT ====================

/// Processor Control Region — the single per-CPU data structure.
///
/// Memory layout designed for optimal SYSCALL performance.
/// GS_BASE points to this structure in kernel mode.
///
/// CRITICAL: Offsets 0-24 are used by assembly — DO NOT CHANGE without updating:
///   - boot/idt_handlers.s (syscall_entry)
///   - core/context_switch.s (context_switch_user)
#[repr(C, align(4096))]
pub struct ProcessorControlRegion {
    // ==================== SYSCALL CRITICAL (fixed offsets) ====================
    // These fields are accessed by assembly via gs:[offset]
    /// Self-reference pointer for GS-based PCR access.
    /// Assembly: `mov rax, gs:[0]` to get PCR pointer.
    pub self_ref: *mut ProcessorControlRegion, // offset 0

    /// Temporary storage for user RSP during SYSCALL entry.
    /// Assembly: `mov gs:[8], rsp` saves user stack.
    pub user_rsp_tmp: u64, // offset 8

    /// Kernel RSP loaded during SYSCALL entry (mirrors TSS.rsp0).
    /// Assembly: `mov rsp, gs:[16]` loads kernel stack.
    pub kernel_rsp: u64, // offset 16

    // ==================== GENERAL PER-CPU DATA ====================
    /// CPU index (0..n-1), NOT the hardware APIC ID.
    /// Assembly: `mov eax, gs:[24]` for fast current_cpu_id().
    pub cpu_id: u32, // offset 24

    /// Hardware Local APIC ID.
    pub apic_id: u32, // offset 28

    /// Preemption disable nesting counter.
    /// >0 means preemption is disabled.
    pub preempt_count: AtomicU32, // offset 32

    /// Currently executing in interrupt/exception context.
    pub in_interrupt: AtomicBool, // offset 36

    _pad1: [u8; 3], // offset 37-39

    /// Pointer to currently running task (opaque).
    pub current_task: AtomicPtr<()>, // offset 40

    /// Pointer to this CPU's scheduler instance (opaque).
    pub scheduler: AtomicPtr<()>, // offset 48

    /// CPU is online and accepting scheduled work.
    pub online: AtomicBool, // offset 56

    _pad2: [u8; 3], // offset 57-59

    /// Deferred reschedule flag (set under preemption-disabled, acted on
    /// when re-enabled).
    pub reschedule_pending: AtomicU32, // offset 60

    // ==================== STATISTICS (cache-line aligned) ====================
    /// Total context switches on this CPU.
    pub context_switches: AtomicU64, // offset 64

    /// Total interrupts handled on this CPU.
    pub interrupt_count: AtomicU64, // offset 72

    /// Total syscalls handled on this CPU.
    pub syscall_count: AtomicU64, // offset 80

    /// PID of task currently in syscall (for user pointer validation).
    pub syscall_pid: AtomicU32, // offset 88

    _pad3: [u8; 4], // offset 92-95

    // ==================== EMBEDDED GDT ====================
    /// Per-CPU Global Descriptor Table.
    /// Contains kernel/user code/data segments + TSS descriptor.
    pub gdt: GdtLayout, // offset 96 (8-byte aligned)

    // Padding to align TSS to 16 bytes.
    _tss_align: [u8; 8],

    // ==================== EMBEDDED TSS ====================
    /// Per-CPU Task State Segment.
    /// TSS.rsp0 = kernel_rsp (kept in sync).
    pub tss: Tss64,

    // ==================== KERNEL STACK ====================
    /// Guard page to catch stack overflow (unmapped or read-only).
    _stack_guard: [u8; 4096],

    /// Per-CPU kernel stack (64KB).
    /// Stack grows down, so kernel_rsp points to end of this array.
    pub kernel_stack: [u8; KERNEL_STACK_SIZE],
}

// Compile-time offset verification.
const _: () = {
    assert!(core::mem::offset_of!(ProcessorControlRegion, self_ref) == 0);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rsp_tmp) == 8);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_rsp) == 16);
    assert!(core::mem::offset_of!(ProcessorControlRegion, cpu_id) == 24);
    assert!(core::mem::offset_of!(ProcessorControlRegion, apic_id) == 28);
    assert!(core::mem::align_of::<ProcessorControlRegion>() == 4096);
};

// SAFETY: PCR uses atomics for all mutable fields and is only
// accessed by the owning CPU (except during initialization).
unsafe impl Send for ProcessorControlRegion {}
unsafe impl Sync for ProcessorControlRegion {}

impl ProcessorControlRegion {
    /// Create a new zeroed PCR.
    pub const fn new() -> Self {
        Self {
            self_ref: ptr::null_mut(),
            user_rsp_tmp: 0,
            kernel_rsp: 0,
            cpu_id: 0,
            apic_id: 0,
            preempt_count: AtomicU32::new(0),
            in_interrupt: AtomicBool::new(false),
            _pad1: [0; 3],
            current_task: AtomicPtr::new(ptr::null_mut()),
            scheduler: AtomicPtr::new(ptr::null_mut()),
            online: AtomicBool::new(false),
            _pad2: [0; 3],
            reschedule_pending: AtomicU32::new(0),
            context_switches: AtomicU64::new(0),
            interrupt_count: AtomicU64::new(0),
            syscall_count: AtomicU64::new(0),
            syscall_pid: AtomicU32::new(u32::MAX),
            _pad3: [0; 4],
            gdt: GdtLayout::new(),
            _tss_align: [0; 8],
            tss: Tss64::new(),
            _stack_guard: [0; 4096],
            kernel_stack: [0; KERNEL_STACK_SIZE],
        }
    }

    /// Get the top of the kernel stack (stack grows down).
    #[inline]
    pub fn kernel_stack_top(&self) -> u64 {
        let stack_base = self.kernel_stack.as_ptr() as u64;
        stack_base + KERNEL_STACK_SIZE as u64
    }

    /// # Safety
    /// Must be called before install().
    pub unsafe fn init_gdt(&mut self) {
        self.gdt.load_standard_entries();
        self.gdt.load_tss(&self.tss);
        self.tss.rsp0 = self.kernel_rsp;
        self.tss.iomap_base = core::mem::size_of::<Tss64>() as u16;
    }

    /// Load this PCR's GDT/TSS and set GS_BASE to point here.
    ///
    /// # Safety
    /// `init_gdt()` must be called first.
    pub unsafe fn install(&mut self) {
        let gdtr = GdtDescriptor::from_layout(&self.gdt);

        core::arch::asm!(
            "lgdt [{0}]",
            in(reg) &gdtr,
            options(nostack, preserves_flags)
        );

        core::arch::asm!(
            "pushq ${code}",
            "lea 2f(%rip), %rax",
            "pushq %rax",
            "lretq",
            "2:",
            "movw ${data}, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "movw %ax, %fs",
            "movw %ax, %gs",
            code = const SegmentSelector::KERNEL_CODE.bits() as usize,
            data = const SegmentSelector::KERNEL_DATA.bits() as usize,
            out("rax") _,
            options(att_syntax, nostack)
        );

        let tss_sel = SegmentSelector::TSS.bits();
        core::arch::asm!(
            "ltr {0:x}",
            in(reg) tss_sel,
            options(nostack, preserves_flags)
        );

        let self_addr = self as *mut _ as u64;
        crate::cpu::write_msr(Msr::GS_BASE, self_addr);
        crate::cpu::write_msr(Msr::KERNEL_GS_BASE, self_addr);

        mark_gs_base_set();
    }

    pub fn sync_tss_rsp0(&mut self) {
        self.tss.rsp0 = self.kernel_rsp;
    }

    /// Set an IST entry.
    pub fn set_ist(&mut self, index: u8, stack_top: u64) {
        if index >= 1 && index <= 7 {
            self.tss.ist[(index - 1) as usize] = stack_top;
        }
    }
}

/// PCR offset constants for assembly code.
pub mod offsets {
    /// Offset of self_ref field (pointer to PCR itself).
    pub const SELF_REF: usize = 0;
    /// Offset of user_rsp_tmp field (user RSP scratch during SYSCALL).
    pub const USER_RSP_TMP: usize = 8;
    /// Offset of kernel_rsp field (kernel RSP for SYSCALL entry).
    pub const KERNEL_RSP: usize = 16;
    /// Offset of cpu_id field (CPU index, not APIC ID).
    pub const CPU_ID: usize = 24;
    /// Offset of apic_id field (hardware APIC ID).
    pub const APIC_ID: usize = 28;
}

// ==================== STATIC STORAGE ====================

/// BSP's PCR (statically allocated).
static BSP_PCR: SyncUnsafeCell<ProcessorControlRegion> =
    SyncUnsafeCell::new(ProcessorControlRegion::new());

/// Statically-allocated AP PCRs.
static AP_PCRS: SyncUnsafeCell<[ProcessorControlRegion; MAX_STATIC_APS]> = SyncUnsafeCell::new({
    const INIT: ProcessorControlRegion = ProcessorControlRegion::new();
    [INIT; MAX_STATIC_APS]
});

/// Wrapper to allow `[*mut ProcessorControlRegion; N]` in a static.
/// Raw pointers are not `Sync`; this is safe because all access is
/// already guarded by single-writer semantics during boot init.
#[repr(transparent)]
struct PcrPtrArray([*mut ProcessorControlRegion; MAX_CPUS]);
unsafe impl Sync for PcrPtrArray {}

static ALL_PCRS: SyncUnsafeCell<PcrPtrArray> =
    SyncUnsafeCell::new(PcrPtrArray([ptr::null_mut(); MAX_CPUS]));

/// Number of initialized PCRs.
static PCR_COUNT: AtomicU32 = AtomicU32::new(0);

static PCR_INIT: InitFlag = InitFlag::new();
static GS_BASE_SET: InitFlag = InitFlag::new();

// ==================== APIC ID ↔ CPU INDEX MAPPING ====================

const INVALID_CPU_IDX: u32 = u32::MAX;

/// Mapping from APIC ID to CPU index.
static APIC_ID_TO_CPU_IDX: [AtomicU32; MAX_CPUS] = {
    const INIT: AtomicU32 = AtomicU32::new(INVALID_CPU_IDX);
    [INIT; MAX_CPUS]
};

/// BSP's APIC ID (set during init).
static BSP_APIC_ID: AtomicU32 = AtomicU32::new(0);

/// Register a bi-directional APIC ID ↔ CPU index mapping.
fn register_apic_mapping(cpu_id: usize, apic_id: u32) {
    if (apic_id as usize) < MAX_CPUS {
        APIC_ID_TO_CPU_IDX[apic_id as usize].store(cpu_id as u32, Ordering::Release);
    }
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
    get_pcr(cpu_id).map(|pcr| pcr.apic_id)
}

/// Get the BSP's APIC ID.
#[inline]
pub fn get_bsp_apic_id() -> u32 {
    BSP_APIC_ID.load(Ordering::Acquire)
}

// ==================== INITIALIZATION ====================

/// Initialize the BSP's PCR: set up data structures, APIC mapping, mark online.
///
/// Does NOT set GS_BASE — call `install()` on the returned PCR for that.
///
/// # Safety
/// Must be called exactly once during early BSP boot.
pub unsafe fn init_bsp_pcr(apic_id: u32) {
    if !PCR_INIT.init_once() {
        return;
    }

    BSP_APIC_ID.store(apic_id, Ordering::Release);

    let pcr = BSP_PCR.get();

    (*pcr).self_ref = pcr;
    (*pcr).cpu_id = 0;
    (*pcr).apic_id = apic_id;
    (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

    (*ALL_PCRS.get()).0[0] = pcr;
    PCR_COUNT.store(1, Ordering::Release);

    // Register APIC mapping and mark BSP online.
    register_apic_mapping(0, apic_id);
    (*pcr).online.store(true, Ordering::Release);
}

/// Initialize a PCR for an Application Processor.
///
/// Returns a pointer to the new PCR. Caller must call `init_gdt()` + `install()`.
///
/// # Safety
/// Must be called exactly once per AP during AP boot.
pub unsafe fn init_ap_pcr(cpu_id: usize, apic_id: u32) -> *mut ProcessorControlRegion {
    if cpu_id == 0 || cpu_id >= MAX_CPUS {
        panic!("init_ap_pcr: invalid cpu_id {}", cpu_id);
    }

    if cpu_id > MAX_STATIC_APS {
        panic!("init_ap_pcr: too many APs (max {})", MAX_STATIC_APS);
    }

    let pcr = &raw mut (*AP_PCRS.get())[cpu_id - 1];

    (*pcr).self_ref = pcr;
    (*pcr).cpu_id = cpu_id as u32;
    (*pcr).apic_id = apic_id;
    (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

    (*ALL_PCRS.get()).0[cpu_id] = pcr;

    let current_count = PCR_COUNT.load(Ordering::Acquire);
    if cpu_id as u32 >= current_count {
        PCR_COUNT.store(cpu_id as u32 + 1, Ordering::Release);
    }

    // Register APIC mapping.
    register_apic_mapping(cpu_id, apic_id);

    pcr
}

pub fn mark_gs_base_set() {
    GS_BASE_SET.init_once();
}

// ==================== CURRENT CPU ACCESS ====================

/// Read the current CPU index via GS segment (fast path, ~1-3 cycles).
///
/// Returns 0 (BSP) if GS_BASE has not been set yet.
#[inline(always)]
pub fn current_cpu_id() -> usize {
    if !GS_BASE_SET.is_set() {
        return 0;
    }
    unsafe {
        let id: u32;
        core::arch::asm!(
            "mov {:e}, gs:[24]",
            out(reg) id,
            options(nostack, preserves_flags, readonly)
        );
        id as usize
    }
}

/// Alias for `current_cpu_id()` — preferred in most call sites.
#[inline(always)]
pub fn get_current_cpu() -> usize {
    current_cpu_id()
}

/// Get the current CPU's PCR via GS segment (fast path).
///
/// # Safety
/// GS_BASE must be set to point to a valid PCR (done during CPU init).
#[inline(always)]
pub unsafe fn current_pcr() -> &'static ProcessorControlRegion {
    let ptr: *mut ProcessorControlRegion;
    core::arch::asm!(
        "mov {}, gs:[0]",
        out(reg) ptr,
        options(nostack, preserves_flags, readonly)
    );
    &*ptr
}

/// Get the current CPU's PCR as mutable via GS segment.
///
/// # Safety
/// GS_BASE must be set to point to a valid PCR.
/// Caller must ensure exclusive access.
#[inline(always)]
pub unsafe fn current_pcr_mut() -> &'static mut ProcessorControlRegion {
    let ptr: *mut ProcessorControlRegion;
    core::arch::asm!(
        "mov {}, gs:[0]",
        out(reg) ptr,
        options(nostack, preserves_flags, readonly)
    );
    &mut *ptr
}

// ==================== PCR LOOKUP BY CPU ID ====================

/// Get a PCR by CPU ID.
///
/// Returns `None` if `cpu_id` is invalid or the PCR has not been initialized.
pub fn get_pcr(cpu_id: usize) -> Option<&'static ProcessorControlRegion> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    unsafe {
        let ptr = (*ALL_PCRS.get()).0[cpu_id];
        if ptr.is_null() {
            None
        } else {
            Some(&*ptr)
        }
    }
}

/// Get a mutable PCR by CPU ID.
///
/// # Safety
/// Caller must ensure exclusive access to the PCR.
pub unsafe fn get_pcr_mut(cpu_id: usize) -> Option<&'static mut ProcessorControlRegion> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    let ptr = (*ALL_PCRS.get()).0[cpu_id];
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

/// Get the number of initialized PCRs (i.e. CPU count).
#[inline]
pub fn get_pcr_count() -> usize {
    PCR_COUNT.load(Ordering::Acquire) as usize
}

/// Check if PCR subsystem is initialized.
#[inline]
pub fn is_pcr_initialized() -> bool {
    PCR_INIT.is_set()
}

// ==================== CPU COUNT & STATE ====================

/// Get the number of initialized CPUs (alias for `get_pcr_count`).
#[inline]
pub fn get_cpu_count() -> usize {
    get_pcr_count()
}

/// Get the number of online (scheduler-running) CPUs.
#[inline]
pub fn get_online_cpu_count() -> usize {
    let count = get_cpu_count().min(MAX_CPUS);
    let mut online = 0;
    for i in 0..count {
        if let Some(pcr) = get_pcr(i) {
            if pcr.online.load(Ordering::Relaxed) {
                online += 1;
            }
        }
    }
    online
}

/// Check if the current CPU is the BSP.
#[inline]
pub fn is_bsp() -> bool {
    get_current_cpu() == 0
}

/// Mark a CPU as online (ready to run tasks).
pub fn mark_cpu_online(cpu_id: usize) {
    if let Some(pcr) = get_pcr(cpu_id) {
        pcr.online.store(true, Ordering::Release);
    }
}

/// Mark a CPU as offline.
pub fn mark_cpu_offline(cpu_id: usize) {
    if let Some(pcr) = get_pcr(cpu_id) {
        pcr.online.store(false, Ordering::Release);
    }
}

/// Check if a CPU is online.
#[inline]
pub fn is_cpu_online(cpu_id: usize) -> bool {
    get_pcr(cpu_id)
        .map(|pcr| pcr.online.load(Ordering::Acquire))
        .unwrap_or(false)
}

// ==================== PER-CPU DATA ACCESSORS ====================
// These operate on the *current* CPU's PCR via GS_BASE.

/// Set the current task pointer for this CPU.
#[inline]
pub fn set_current_task(task: *mut ()) {
    if !GS_BASE_SET.is_set() {
        return;
    }
    unsafe {
        current_pcr().current_task.store(task, Ordering::Release);
    }
}

/// Get the current task pointer for this CPU.
#[inline]
pub fn get_current_task() -> *mut () {
    if !GS_BASE_SET.is_set() {
        return ptr::null_mut();
    }
    unsafe { current_pcr().current_task.load(Ordering::Acquire) }
}

/// Increment the context switch counter for this CPU.
#[inline]
pub fn increment_context_switches() {
    if !GS_BASE_SET.is_set() {
        return;
    }
    unsafe {
        current_pcr()
            .context_switches
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Increment the interrupt counter for this CPU.
#[inline]
pub fn increment_interrupt_count() {
    if !GS_BASE_SET.is_set() {
        return;
    }
    unsafe {
        current_pcr()
            .interrupt_count
            .fetch_add(1, Ordering::Relaxed);
    }
}

// ==================== IPI INFRASTRUCTURE ====================

pub type SendIpiToCpuFn = fn(u32, u8);

static SEND_IPI_TO_CPU_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static LAPIC_ID_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Register the IPI send function from the APIC driver.
pub fn register_send_ipi_to_cpu_fn(f: SendIpiToCpuFn) {
    SEND_IPI_TO_CPU_FN.store(f as *mut (), Ordering::Release);
}

/// Send an IPI to the specified CPU.
pub fn send_ipi_to_cpu(target_apic_id: u32, vector: u8) {
    let fn_ptr = SEND_IPI_TO_CPU_FN.load(Ordering::Acquire);
    if !fn_ptr.is_null() {
        let f: SendIpiToCpuFn = unsafe { core::mem::transmute(fn_ptr) };
        f(target_apic_id, vector);
    }
}

/// Register the LAPIC ID reader function from the APIC driver.
pub fn register_lapic_id_fn(f: fn() -> u32) {
    LAPIC_ID_FN.store(f as *mut (), Ordering::Release);
}

// ==================== NMI IPI ====================

pub type SendNmiToCpuFn = fn(u32);

static SEND_NMI_TO_CPU_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Register the NMI send function from the APIC driver.
pub fn register_send_nmi_fn(f: SendNmiToCpuFn) {
    SEND_NMI_TO_CPU_FN.store(f as *mut (), Ordering::Release);
}

/// Send an NMI to the specified CPU (by APIC ID).
///
/// Used by the NMI watchdog to interrupt a CPU that appears stuck with
/// interrupts disabled.  No-op if the NMI send function has not been
/// registered yet.
pub fn send_nmi_to_cpu(target_apic_id: u32) {
    let fn_ptr = SEND_NMI_TO_CPU_FN.load(Ordering::Acquire);
    if !fn_ptr.is_null() {
        let f: SendNmiToCpuFn = unsafe { core::mem::transmute(fn_ptr) };
        f(target_apic_id);
    }
}
