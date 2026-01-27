//! Unified Processor Control Region (PCR) for SMP Support
//!
//! This module provides a single unified per-CPU data structure following Redox OS patterns.
//! The PCR consolidates all per-CPU state including:
//! - SYSCALL scratch space (user_rsp_tmp, kernel_rsp)
//! - CPU identification (cpu_id, apic_id)
//! - Scheduler state (current_task, scheduler, preempt_count)
//! - GDT and TSS (embedded in PCR)
//! - Per-CPU kernel stack
//!
//! # GS_BASE Discipline
//!
//! In kernel mode, `GS_BASE` always points to the current CPU's PCR.
//! This is maintained by:
//! - `swapgs` on syscall/interrupt entry from user mode
//! - `swapgs` on syscall/interrupt exit to user mode
//! - `context_switch_user` setting up MSRs correctly before `iretq`
//!
//! # Assembly Offsets (CRITICAL)
//!
//! Fields at offsets 0-24 are accessed by assembly code via `gs:[offset]`.
//! DO NOT CHANGE these field positions without updating:
//! - `boot/idt_handlers.s` (syscall_entry)
//! - `core/context_switch.s` (context_switch_user)

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

/// Kernel stack size per CPU (64KB)
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Maximum number of CPUs supported
pub const PCR_MAX_CPUS: usize = 256;

/// GDT entry count: null, kernel code, kernel data, user data, user code = 5
/// Plus TSS descriptor (2 entries for 64-bit TSS)
pub const GDT_ENTRY_COUNT: usize = 7;

/// 64-bit Task State Segment
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tss64 {
    pub reserved0: u32,
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    pub reserved1: u64,
    pub ist: [u64; 7],
    pub reserved2: u64,
    pub reserved3: u16,
    pub iomap_base: u16,
}

impl Tss64 {
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: 0,
        }
    }
}

/// TSS descriptor entry (16 bytes for 64-bit mode)
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct GdtTssEntry {
    pub limit_low: u16,
    pub base_low: u16,
    pub base_mid: u8,
    pub access: u8,
    pub granularity: u8,
    pub base_high: u8,
    pub base_upper: u32,
    pub reserved: u32,
}

impl GdtTssEntry {
    pub const fn new() -> Self {
        Self {
            limit_low: 0,
            base_low: 0,
            base_mid: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
            base_upper: 0,
            reserved: 0,
        }
    }
}

/// GDT layout with embedded TSS descriptor
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct GdtLayout {
    /// Standard GDT entries: null, kernel code, kernel data, user data, user code
    pub entries: [u64; 5],
    /// TSS descriptor (16 bytes)
    pub tss_entry: GdtTssEntry,
}

impl GdtLayout {
    pub const fn new() -> Self {
        Self {
            entries: [0; 5],
            tss_entry: GdtTssEntry::new(),
        }
    }
}

/// Processor Control Region - unified per-CPU data structure
///
/// Memory layout designed for optimal SYSCALL performance.
/// GS_BASE points to this structure in kernel mode.
///
/// CRITICAL: Offsets 0-24 are used by assembly - DO NOT CHANGE without updating:
///   - boot/idt_handlers.s (syscall_entry)
///   - core/context_switch.s (context_switch_user)
#[repr(C, align(4096))]
pub struct ProcessorControlRegion {
    // ==================== SYSCALL CRITICAL (fixed offsets) ====================
    // These fields are accessed by assembly via gs:[offset]
    /// Self-reference pointer for GS-based PCR access
    /// Assembly: `mov rax, gs:[0]` to get PCR pointer
    pub self_ref: *mut ProcessorControlRegion, // offset 0

    /// Temporary storage for user RSP during SYSCALL entry
    /// Assembly: `mov gs:[8], rsp` saves user stack
    pub user_rsp_tmp: u64, // offset 8

    /// Kernel RSP loaded during SYSCALL entry (mirrors TSS.rsp0)
    /// Assembly: `mov rsp, gs:[16]` loads kernel stack
    pub kernel_rsp: u64, // offset 16

    // ==================== GENERAL PER-CPU DATA ====================
    /// CPU index (0..n-1), NOT the hardware APIC ID
    /// Assembly: `mov eax, gs:[24]` for fast current_cpu_id()
    pub cpu_id: u32, // offset 24

    /// Hardware Local APIC ID
    pub apic_id: u32, // offset 28

    /// Preemption disable nesting counter
    /// >0 means preemption is disabled
    pub preempt_count: AtomicU32, // offset 32

    /// Currently executing in interrupt/exception context
    pub in_interrupt: AtomicBool, // offset 36

    _pad1: [u8; 3], // offset 37-39

    /// Pointer to currently running task (opaque)
    pub current_task: AtomicPtr<()>, // offset 40

    /// Pointer to this CPU's scheduler instance (opaque)
    pub scheduler: AtomicPtr<()>, // offset 48

    /// CPU is online and accepting scheduled work
    pub online: AtomicBool, // offset 56

    _pad2: [u8; 7], // offset 57-63

    // ==================== STATISTICS (cache-line aligned) ====================
    /// Total context switches on this CPU
    pub context_switches: AtomicU64, // offset 64

    /// Total interrupts handled on this CPU
    pub interrupt_count: AtomicU64, // offset 72

    /// Total syscalls handled on this CPU
    pub syscall_count: AtomicU64, // offset 80

    /// PID of task currently in syscall (for user pointer validation)
    pub syscall_pid: AtomicU32, // offset 88

    _pad3: [u8; 4], // offset 92-95

    // ==================== EMBEDDED GDT ====================
    /// Per-CPU Global Descriptor Table
    /// Contains kernel/user code/data segments + TSS descriptor
    pub gdt: GdtLayout, // offset 96 (8-byte aligned)

    // Padding to align TSS to 16 bytes
    _tss_align: [u8; 8],

    // ==================== EMBEDDED TSS ====================
    /// Per-CPU Task State Segment
    /// TSS.rsp0 = kernel_rsp (kept in sync)
    pub tss: Tss64,

    // ==================== KERNEL STACK ====================
    /// Guard page to catch stack overflow (unmapped or read-only)
    _stack_guard: [u8; 4096],

    /// Per-CPU kernel stack (64KB)
    /// Stack grows down, so kernel_rsp points to end of this array
    pub kernel_stack: [u8; KERNEL_STACK_SIZE],
}

// Compile-time offset verification
const _: () = {
    assert!(core::mem::offset_of!(ProcessorControlRegion, self_ref) == 0);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rsp_tmp) == 8);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_rsp) == 16);
    assert!(core::mem::offset_of!(ProcessorControlRegion, cpu_id) == 24);
    assert!(core::mem::offset_of!(ProcessorControlRegion, apic_id) == 28);
    assert!(core::mem::align_of::<ProcessorControlRegion>() == 4096);
};

impl ProcessorControlRegion {
    /// Create a new zeroed PCR
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
            _pad2: [0; 7],
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

    /// Get the top of the kernel stack (stack grows down)
    #[inline]
    pub fn kernel_stack_top(&self) -> u64 {
        let stack_base = self.kernel_stack.as_ptr() as u64;
        stack_base + KERNEL_STACK_SIZE as u64
    }
}

// SAFETY: PCR uses atomics for all mutable fields and is only
// accessed by the owning CPU (except during initialization).
unsafe impl Send for ProcessorControlRegion {}
unsafe impl Sync for ProcessorControlRegion {}

// ==================== GDT/TSS INITIALIZATION ====================

const GDT_ACCESS_PRESENT: u8 = 1 << 7;
const GDT_ACCESS_DPL_KERNEL: u8 = 0 << 5;
const GDT_ACCESS_DPL_USER: u8 = 3 << 5;
const GDT_ACCESS_SEGMENT: u8 = 1 << 4;
const GDT_ACCESS_CODE_TYPE: u8 = 0b1010;
const GDT_ACCESS_DATA_TYPE: u8 = 0b0010;
const GDT_FLAG_GRANULARITY: u8 = 1 << 3;
const GDT_FLAG_LONG_MODE: u8 = 1 << 1;
const GDT_FLAGS_64BIT: u8 = GDT_FLAG_GRANULARITY | GDT_FLAG_LONG_MODE;

const fn gdt_make_descriptor(
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    limit_high: u8,
    flags: u8,
    base_high: u8,
) -> u64 {
    (limit_low as u64)
        | ((base_low as u64) << 16)
        | ((base_mid as u64) << 32)
        | ((access as u64) << 40)
        | ((limit_high as u64) << 48)
        | ((flags as u64) << 52)
        | ((base_high as u64) << 56)
}

const GDT_NULL_DESCRIPTOR: u64 = 0;
const GDT_CODE_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);
const GDT_DATA_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);
const GDT_USER_DATA_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);
const GDT_USER_CODE_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);

pub const GDT_KERNEL_CS: u16 = 0x08;
pub const GDT_KERNEL_DS: u16 = 0x10;
pub const GDT_USER_DS: u16 = 0x1B;
pub const GDT_USER_CS: u16 = 0x23;
pub const GDT_TSS_SELECTOR: u16 = 0x28;

const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u64,
}

impl ProcessorControlRegion {
    /// Initialize GDT and TSS entries in this PCR
    ///
    /// # Safety
    /// Must be called before install()
    pub unsafe fn init_gdt(&mut self) {
        self.gdt.entries[0] = GDT_NULL_DESCRIPTOR;
        self.gdt.entries[1] = GDT_CODE_DESCRIPTOR_64;
        self.gdt.entries[2] = GDT_DATA_DESCRIPTOR_64;
        self.gdt.entries[3] = GDT_USER_DATA_DESCRIPTOR_64;
        self.gdt.entries[4] = GDT_USER_CODE_DESCRIPTOR_64;

        let tss_base = &self.tss as *const _ as u64;
        let tss_limit = core::mem::size_of::<Tss64>() as u16 - 1;

        self.gdt.tss_entry.limit_low = tss_limit & 0xFFFF;
        self.gdt.tss_entry.base_low = (tss_base & 0xFFFF) as u16;
        self.gdt.tss_entry.base_mid = ((tss_base >> 16) & 0xFF) as u8;
        self.gdt.tss_entry.access = 0x89;
        self.gdt.tss_entry.granularity = (((tss_limit as u32) >> 16) & 0x0F) as u8;
        self.gdt.tss_entry.base_high = ((tss_base >> 24) & 0xFF) as u8;
        self.gdt.tss_entry.base_upper = (tss_base >> 32) as u32;
        self.gdt.tss_entry.reserved = 0;

        self.tss.rsp0 = self.kernel_rsp;
        self.tss.iomap_base = core::mem::size_of::<Tss64>() as u16;
    }

    /// Load this PCR's GDT and configure GS_BASE
    ///
    /// # Safety
    /// init_gdt() must be called first
    pub unsafe fn install(&mut self) {
        let gdtr = GdtDescriptor {
            limit: (core::mem::size_of::<GdtLayout>() - 1) as u16,
            base: &self.gdt as *const _ as u64,
        };

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
            code = const GDT_KERNEL_CS as usize,
            data = const GDT_KERNEL_DS as usize,
            out("rax") _,
            options(att_syntax, nostack)
        );

        let tss_sel = GDT_TSS_SELECTOR;
        core::arch::asm!(
            "ltr {0:x}",
            in(reg) tss_sel,
            options(nostack, preserves_flags)
        );

        let self_addr = self as *mut _ as u64;
        let low = self_addr as u32;
        let high = (self_addr >> 32) as u32;

        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_GS_BASE,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags)
        );

        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_KERNEL_GS_BASE,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags)
        );

        mark_gs_base_set();
    }

    pub fn sync_tss_rsp0(&mut self) {
        self.tss.rsp0 = self.kernel_rsp;
    }

    /// Set an IST entry
    pub fn set_ist(&mut self, index: u8, stack_top: u64) {
        if index >= 1 && index <= 7 {
            self.tss.ist[(index - 1) as usize] = stack_top;
        }
    }
}

/// PCR offset constants for assembly code
pub mod offsets {
    /// Offset of self_ref field (pointer to PCR itself)
    pub const SELF_REF: usize = 0;
    /// Offset of user_rsp_tmp field (user RSP scratch during SYSCALL)
    pub const USER_RSP_TMP: usize = 8;
    /// Offset of kernel_rsp field (kernel RSP for SYSCALL entry)
    pub const KERNEL_RSP: usize = 16;
    /// Offset of cpu_id field (CPU index, not APIC ID)
    pub const CPU_ID: usize = 24;
    /// Offset of apic_id field (hardware APIC ID)
    pub const APIC_ID: usize = 28;
}

// ==================== PCR STORAGE AND ACCESS ====================

use crate::InitFlag;

/// BSP's PCR (statically allocated)
static mut BSP_PCR: ProcessorControlRegion = ProcessorControlRegion::new();

/// Array of pointers to all PCRs (BSP + APs)
/// Index 0 = BSP, Index 1+ = APs
static mut ALL_PCRS: [*mut ProcessorControlRegion; PCR_MAX_CPUS] = [ptr::null_mut(); PCR_MAX_CPUS];

/// Number of initialized PCRs
static PCR_COUNT: AtomicU32 = AtomicU32::new(0);

static PCR_INIT: InitFlag = InitFlag::new();
static GS_BASE_SET: InitFlag = InitFlag::new();

/// Initialize the BSP's PCR (data structures only, GS_BASE not yet set)
///
/// # Safety
/// Must be called exactly once during early BSP boot.
/// Must call `install()` on the returned PCR before using `current_cpu_id()`.
pub unsafe fn init_bsp_pcr(apic_id: u32) {
    if !PCR_INIT.init_once() {
        return;
    }

    let pcr = &raw mut BSP_PCR;

    (*pcr).self_ref = pcr;
    (*pcr).cpu_id = 0;
    (*pcr).apic_id = apic_id;
    (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

    ALL_PCRS[0] = pcr;
    PCR_COUNT.store(1, Ordering::Release);
}

pub fn mark_gs_base_set() {
    GS_BASE_SET.init_once();
}

/// Allocate and initialize a PCR for an AP
///
/// Returns a pointer to the new PCR.
///
/// # Safety
/// Must be called exactly once per AP during AP boot.
pub unsafe fn init_ap_pcr(cpu_id: usize, apic_id: u32) -> *mut ProcessorControlRegion {
    if cpu_id == 0 || cpu_id >= PCR_MAX_CPUS {
        panic!("init_ap_pcr: invalid cpu_id {}", cpu_id);
    }

    // Allocate PCR from heap (page-aligned)
    // For now, we use a simple static array for APs as well
    // In a production kernel, this would use the page allocator
    static mut AP_PCRS: [ProcessorControlRegion; 16] = {
        const INIT: ProcessorControlRegion = ProcessorControlRegion::new();
        [INIT; 16]
    };

    if cpu_id > 16 {
        panic!("init_ap_pcr: too many APs (max 16)");
    }

    let pcr = &raw mut AP_PCRS[cpu_id - 1];

    // Set up self-reference
    (*pcr).self_ref = pcr;

    // Set CPU and APIC IDs
    (*pcr).cpu_id = cpu_id as u32;
    (*pcr).apic_id = apic_id;

    // Calculate kernel stack top (stack grows down)
    (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

    ALL_PCRS[cpu_id] = pcr;

    let current_count = PCR_COUNT.load(Ordering::Acquire);
    if cpu_id as u32 >= current_count {
        PCR_COUNT.store(cpu_id as u32 + 1, Ordering::Release);
    }

    pcr
}

/// Get the current CPU's PCR via GS segment (FAST PATH - ~1-3 cycles)
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

/// Get the current CPU's PCR as mutable via GS segment
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

/// Get a PCR by CPU ID
///
/// # Safety
/// Returns None if cpu_id is invalid or PCR not initialized.
pub fn get_pcr(cpu_id: usize) -> Option<&'static ProcessorControlRegion> {
    if cpu_id >= PCR_MAX_CPUS {
        return None;
    }
    unsafe {
        let ptr = ALL_PCRS[cpu_id];
        if ptr.is_null() { None } else { Some(&*ptr) }
    }
}

/// Get a mutable PCR by CPU ID
///
/// # Safety
/// Caller must ensure exclusive access to the PCR.
pub unsafe fn get_pcr_mut(cpu_id: usize) -> Option<&'static mut ProcessorControlRegion> {
    if cpu_id >= PCR_MAX_CPUS {
        return None;
    }
    let ptr = ALL_PCRS[cpu_id];
    if ptr.is_null() { None } else { Some(&mut *ptr) }
}

/// Get the number of initialized PCRs (CPU count)
#[inline]
pub fn get_pcr_count() -> usize {
    PCR_COUNT.load(Ordering::Acquire) as usize
}

/// Check if PCR subsystem is initialized
#[inline]
pub fn is_pcr_initialized() -> bool {
    PCR_INIT.is_set()
}
