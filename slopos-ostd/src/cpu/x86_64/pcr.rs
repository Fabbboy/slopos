//! Unified per-CPU data infrastructure for SMP: the `ProcessorControlRegion`
//! (PCR) reached via GS_BASE, APIC ID ↔ CPU index mapping, CPU lifecycle,
//! per-CPU data accessors and IPI callback registration.

use core::arch::naked_asm;
use core::cell::SyncUnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::arch::x86_64::gdt::{GdtLayout, SegmentSelector, Tss64};
use crate::arch::x86_64::msr::Msr;

use crate::sync::BspToken;
use crate::sync::init_flag::InitFlag;
use crate::user::context::UserContext;

pub const MAX_CPUS: usize = 256;

pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

const MAX_STATIC_APS: usize = 16;

/// Processor Control Region — the single per-CPU data structure; GS_BASE points
/// here in kernel mode.
///
/// Offsets 0-24 are hard-coded in assembly: changing them requires updating
/// `slopos-ostd/src/user/asm/user_return.s` and `core/context_switch.s`.
#[repr(C, align(4096))]
pub struct ProcessorControlRegion {
    /// Self-reference pointer for GS-based PCR access.
    pub self_ref: *mut ProcessorControlRegion, // offset 0

    /// Temporary storage for user RSP during SYSCALL entry.
    pub user_rsp_tmp: u64, // offset 8

    /// Kernel RSP loaded during SYSCALL entry (mirrors TSS.rsp0).
    pub kernel_rsp: u64, // offset 16

    /// CPU index (0..n-1), NOT the hardware APIC ID.
    pub cpu_id: u32, // offset 24

    pub apic_id: u32, // offset 28

    /// Preemption disable nesting counter; >0 means preemption is disabled.
    pub preempt_count: AtomicU32, // offset 32

    pub in_interrupt: AtomicBool, // offset 36

    /// This CPU has bottom-half work waiting for a legal place to run it. Set
    /// by `sync::bh::raise` as one `gs`-relative byte store, so it is legal
    /// from a hard IRQ handler and from under a cli-spinlock.
    pub bh_pending: AtomicBool, // offset 37

    pub bh_active: AtomicBool, // offset 38

    _pad1: [u8; 1], // offset 39

    /// Pointer to currently running task (opaque).
    ///
    /// Read by the SafeStack sanitizer's `__safestack_pointer_address` on every
    /// instrumented function prologue, so it must always name a valid Task (or
    /// bootstrap stub) with a primed `unsafe_stack_sp` whenever instrumented
    /// code may run; nulling it crashes the next prologue.
    pub current_task: AtomicPtr<()>, // offset 40

    /// Pointer to this CPU's idle task (opaque). Written once per CPU, during
    /// `create_idle_task_for_cpu()`.
    pub idle_task: AtomicPtr<()>, // offset 48

    /// CPU is online and accepting scheduled work.
    pub online: AtomicBool, // offset 56

    _pad2: [u8; 3], // offset 57-59

    /// Deferred reschedule flag (set under preemption-disabled, acted on
    /// when re-enabled).
    pub reschedule_pending: AtomicU32, // offset 60

    pub context_switches: AtomicU64, // offset 64

    pub interrupt_count: AtomicU64, // offset 72

    pub syscall_count: AtomicU64, // offset 80

    /// PID of task currently in syscall (for user pointer validation).
    pub syscall_pid: AtomicU32, // offset 88

    _pad3: [u8; 4], // offset 92-95

    /// Per-CPU active `UserContext` pointer.  Set by
    /// `PcrUserModeBackend::execute_round_trip` before iretq into user
    /// mode; consumed by `__ostd_user_return` to write user state back.
    pub user_ctx_ptr: AtomicPtr<UserContext>, // offset 96

    /// Saved kernel callee-save snapshot used by `__ostd_user_return`
    /// to unwind back to the caller of `execute_round_trip`.
    pub kernel_return_ctx: SyncUnsafeCell<KernelReturnContext>, // offset 104

    /// Per-CPU scratch slot for the user RAX value during the
    /// `__ostd_user_return` trampoline.  Spilling RAX onto the kernel stack at
    /// `kernel_rsp - 8` would collide with the next CPU-pushed IRET frame's SS
    /// slot at TSS.RSP0.
    pub user_rax_tmp: SyncUnsafeCell<u64>, // offset 168

    pub gdt: GdtLayout, // offset 192

    // Padding to align TSS to 16 bytes.
    _tss_align: [u8; 8],

    /// TSS.rsp0 = kernel_rsp (kept in sync).
    pub tss: Tss64,

    /// Guard page to catch stack overflow (unmapped or read-only).
    _stack_guard: [u8; 4096],

    /// Stack grows down, so kernel_rsp points to end of this array.
    pub kernel_stack: [u8; KERNEL_STACK_SIZE],

    /// Per-CPU SafeStack **data**-stack pointer for IST/exception context.
    ///
    /// `__safestack_pointer_address` returns the address of THIS slot (instead
    /// of `current_task->unsafe_stack_sp`) whenever the running `RSP` is inside
    /// `EXCEPTION_STACK_REGION`, so instrumented code in an exception handler
    /// walks a dedicated guard-paged per-CPU stack rather than the interrupted
    /// task's small data stack. Primed by `ist_stacks` before interrupts are
    /// enabled and re-primed by `retire_faulted_cpu`, the one exception path
    /// that abandons without unwinding. Appended after the embedded kernel
    /// stack so every asm-critical PCR offset (`<= 184`) and the GDT/TSS layout
    /// stay byte-identical. The cell's inner `u64` sits at its offset 0, so
    /// `offset_of!(PCR, ist_unsafe_sp)` is the address the asm returns.
    pub ist_unsafe_sp: SyncUnsafeCell<u64>,

    /// Reliable Abort Core — per-CPU emergency SAFE-stack top (RSP).
    ///
    /// The fatal-fault trampoline switches `RSP` here before any panic
    /// formatting, so a panic from a near-full safe stack still has headroom.
    /// Primed by `ist_stacks` alongside `ist_unsafe_sp`.
    pub panic_safe_sp: SyncUnsafeCell<u64>,

    /// Reliable Abort Core — per-CPU emergency DATA-stack top (SafeStack).
    ///
    /// The trampoline stores this into the running context's data-SP slot
    /// (`current_task->unsafe_stack_sp` once RSP has left the IST region) so
    /// panic-time `core::fmt` runs on a dedicated guard-paged data stack
    /// instead of overflowing the 16 KiB task data stack.
    pub panic_unsafe_sp: SyncUnsafeCell<u64>,

    /// Reliable Abort Core — per-CPU fault-in-fault recursion depth.
    ///
    /// Bumped on each fatal-panic entry; a value `>= 1` on entry means the
    /// fatal path itself faulted, so the orchestration degrades to the
    /// format-free `panic_abort_raw` rather than recursing through the same
    /// (now-suspect) report.
    pub panic_depth: AtomicU32,

    /// Panic-core entry counter for this CPU.
    ///
    /// Incremented at the top of the panic handler and decremented only when a
    /// caught test panic returns through the unwind boundary. A non-zero prior
    /// value means the panic handler itself re-entered and must use the fixed
    /// abort floor.
    pub panic_in_flight: AtomicU32,

    /// Depth-bearing interrupt/exception nesting counter for this CPU;
    /// `in_interrupt` mirrors it as a one-bit flag for callers that only
    /// need binary state.
    pub interrupt_nesting: AtomicU32,

    /// Owning reference to the task most recently switched off this CPU.
    ///
    /// The context-switch tail installs the outgoing reference while running
    /// on the idle stack. The dispatcher takes and releases it exactly once
    /// after leaving the IRQ-off switch window.
    pub previous_task: AtomicPtr<()>,

    /// Panic-recovery nesting depth for this CPU: nested catch scopes unwind
    /// one level at a time, and only the CPU that entered recovery observes
    /// it as active.
    pub recovery_depth: AtomicU32,

    /// ID of the task `current_task` names, republished by the same
    /// `dispatch()` that writes the pointer.
    ///
    /// Exists so the many callers that want only "who am I" never dereference
    /// the task, whose pointer may name a pre-heap bootstrap stub.
    /// `INVALID_TASK_ID` until this CPU dispatches for the first time.
    pub current_task_id: AtomicU32,

    /// Scheduling priority of the task `current_task` names, republished by the
    /// same [`set_current_task`] that writes the pointer and the id.
    ///
    /// Exists so a wake publisher can ask "would the newcomer preempt what is
    /// running over there?" without dereferencing a *foreign* CPU's task: that
    /// dereference races the target CPU's switch tail, which releases the
    /// outgoing dispatch reference and can run its destructor.
    ///
    /// [`PRIORITY_NONE`] until this CPU dispatches for the first time; after
    /// that it always names a real task, since an idle CPU parks on its idle
    /// task and publishes `TaskPriority::Idle` rather than the sentinel.
    pub current_task_priority: AtomicU8,

    /// Monotonic progress counter for the lockup detector.
    ///
    /// Bumped from the timer tick before any lock is taken, and from
    /// [`crate::watchdog::touch`] inside the few bounded loops that run
    /// long enough to outlast a tick. A watcher compares it against its own
    /// previous reading, so the detector does no clock arithmetic and
    /// cannot be fooled by emulation or host steal time.
    pub heartbeat: AtomicU64,

    /// Whether this CPU's LAPIC timer is running periodically.
    ///
    /// A CPU is marked online before it starts its timer, so `online` alone
    /// would have the detector watch a CPU that cannot yet tick. A zero
    /// heartbeat cannot stand in for this: [`crate::watchdog::touch`] makes it
    /// non-zero without a timer ever having fired.
    pub timer_armed: AtomicBool,

    /// Set while this CPU is deliberately running without timer ticks, by
    /// a [`crate::watchdog::Suppress`] token.
    pub watchdog_suppressed: AtomicBool,
}

/// `current_task_priority` for a CPU that is running nothing schedulable.
///
/// Numerically worst, because `TaskPriority` orders `High = 0` upward: any real
/// task outranks it.
pub const PRIORITY_NONE: u8 = u8::MAX;

const _: () = {
    assert!(core::mem::offset_of!(ProcessorControlRegion, self_ref) == 0);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rsp_tmp) == 8);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_rsp) == 16);
    assert!(core::mem::offset_of!(ProcessorControlRegion, cpu_id) == 24);
    assert!(core::mem::offset_of!(ProcessorControlRegion, apic_id) == 28);
    assert!(core::mem::offset_of!(ProcessorControlRegion, preempt_count) == 32);
    // The bottom-half bytes share `preempt_count`'s cache line: they are read
    // on the same edge, at every outermost unlock.
    assert!(core::mem::offset_of!(ProcessorControlRegion, in_interrupt) == 36);
    assert!(core::mem::offset_of!(ProcessorControlRegion, bh_pending) == 37);
    assert!(core::mem::offset_of!(ProcessorControlRegion, bh_active) == 38);
    assert!(core::mem::offset_of!(ProcessorControlRegion, reschedule_pending) == 60);
    assert!(core::mem::offset_of!(ProcessorControlRegion, syscall_pid) == 88);
    assert!(core::mem::offset_of!(ProcessorControlRegion, current_task) == 40);
    assert!(core::mem::offset_of!(ProcessorControlRegion, idle_task) == 48);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_ctx_ptr) == 96);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_return_ctx) == 104);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rax_tmp) == 168);
    assert!(core::mem::align_of::<ProcessorControlRegion>() == 4096);
};

/// Saved kernel callee-save snapshot used by `__ostd_user_return` to
/// unwind back to the caller of `PcrUserModeBackend::execute_round_trip`.
#[repr(C)]
#[derive(Default)]
pub struct KernelReturnContext {
    pub rbx: u64, // offset 0
    pub rbp: u64, // offset 8
    pub r12: u64, // offset 16
    pub r13: u64, // offset 24
    pub r14: u64, // offset 32
    pub r15: u64, // offset 40
    /// RSP value to restore on user→kernel return.  Restored before
    /// the trampoline `jmp`s back so the caller's `ret` finds an
    /// intact frame.
    pub rsp: u64, // offset 48
    /// RIP to `jmp` to on user→kernel return.  Address of the asm
    /// label immediately after `iretq` inside `execute_round_trip`.
    pub rip: u64, // offset 56
}

const _: () = {
    assert!(core::mem::offset_of!(KernelReturnContext, rbx) == 0);
    assert!(core::mem::offset_of!(KernelReturnContext, rbp) == 8);
    assert!(core::mem::offset_of!(KernelReturnContext, r12) == 16);
    assert!(core::mem::offset_of!(KernelReturnContext, r13) == 24);
    assert!(core::mem::offset_of!(KernelReturnContext, r14) == 32);
    assert!(core::mem::offset_of!(KernelReturnContext, r15) == 40);
    assert!(core::mem::offset_of!(KernelReturnContext, rsp) == 48);
    assert!(core::mem::offset_of!(KernelReturnContext, rip) == 56);
    assert!(core::mem::size_of::<KernelReturnContext>() == 64);
};

// SAFETY: PCR uses atomics for all mutable fields and is only
// accessed by the owning CPU (except during initialization).
unsafe impl Send for ProcessorControlRegion {}
unsafe impl Sync for ProcessorControlRegion {}

impl ProcessorControlRegion {
    pub const fn new() -> Self {
        Self {
            self_ref: ptr::null_mut(),
            user_rsp_tmp: 0,
            kernel_rsp: 0,
            cpu_id: 0,
            apic_id: 0,
            preempt_count: AtomicU32::new(0),
            in_interrupt: AtomicBool::new(false),
            bh_pending: AtomicBool::new(false),
            bh_active: AtomicBool::new(false),
            _pad1: [0; 1],
            current_task: AtomicPtr::new(ptr::null_mut()),
            idle_task: AtomicPtr::new(ptr::null_mut()),
            online: AtomicBool::new(false),
            _pad2: [0; 3],
            reschedule_pending: AtomicU32::new(0),
            context_switches: AtomicU64::new(0),
            interrupt_count: AtomicU64::new(0),
            syscall_count: AtomicU64::new(0),
            syscall_pid: AtomicU32::new(u32::MAX),
            _pad3: [0; 4],
            user_ctx_ptr: AtomicPtr::new(ptr::null_mut()),
            kernel_return_ctx: SyncUnsafeCell::new(KernelReturnContext {
                rbx: 0,
                rbp: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                rsp: 0,
                rip: 0,
            }),
            user_rax_tmp: SyncUnsafeCell::new(0),
            gdt: GdtLayout::new(),
            _tss_align: [0; 8],
            tss: Tss64::new(),
            _stack_guard: [0; 4096],
            kernel_stack: [0; KERNEL_STACK_SIZE],
            ist_unsafe_sp: SyncUnsafeCell::new(0),
            panic_safe_sp: SyncUnsafeCell::new(0),
            panic_unsafe_sp: SyncUnsafeCell::new(0),
            panic_depth: AtomicU32::new(0),
            panic_in_flight: AtomicU32::new(0),
            interrupt_nesting: AtomicU32::new(0),
            previous_task: AtomicPtr::new(ptr::null_mut()),
            recovery_depth: AtomicU32::new(0),
            current_task_id: AtomicU32::new(u32::MAX),
            current_task_priority: AtomicU8::new(PRIORITY_NONE),
            heartbeat: AtomicU64::new(0),
            timer_armed: AtomicBool::new(false),
            watchdog_suppressed: AtomicBool::new(false),
        }
    }

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
        // SAFETY (Inv. 2): self.gdt is valid for the PCR's lifetime
        // and the TSS descriptor inside it was populated by init_gdt().
        crate::arch::x86_64::gdt::install(&self.gdt, SegmentSelector::TSS);

        let self_addr = self as *mut _ as u64;
        crate::arch::x86_64::msr::write_msr(Msr::GS_BASE, self_addr);
        crate::arch::x86_64::msr::write_msr(Msr::KERNEL_GS_BASE, self_addr);

        mark_gs_base_set();
    }

    pub fn sync_tss_rsp0(&mut self) {
        self.tss.rsp0 = self.kernel_rsp;
    }

    /// Safe `init_gdt()` + `install()` pair for the BSP-init scope.
    ///
    /// `&BspToken<'brand>` discharges both contracts: this fn pairs the two
    /// calls in the required order, and the BSP-init scope guarantees the PCR
    /// was just minted, so the `&mut self` borrow is the unique-owner reborrow
    /// boot performs once per CPU.
    pub fn bsp_init_gdt_and_install<'brand>(&mut self, _token: &crate::sync::BspToken<'brand>) {
        // SAFETY: `init_gdt` then `install` pairs the two halves; the
        // `&BspToken` witnesses BSP-init scope (pre-SMP, BSP only).
        unsafe {
            self.init_gdt();
            self.install();
        }
    }

    pub fn set_ist(&mut self, index: u8, stack_top: u64) {
        if index >= 1 && index <= 7 {
            self.tss.ist[(index - 1) as usize] = stack_top;
        }
    }
}

/// PCR offset constants for assembly code.
pub mod offsets {
    pub const SELF_REF: usize = 0;
    pub const USER_RSP_TMP: usize = 8;
    pub const KERNEL_RSP: usize = 16;
    pub const CPU_ID: usize = 24;
    pub const APIC_ID: usize = 28;
    pub const PREEMPT_COUNT: usize = 32;
    pub const BH_PENDING: usize = 37;
    pub const RESCHEDULE_PENDING: usize = 60;
    pub const CURRENT_TASK: usize = 40;
    pub const IDLE_TASK: usize = 48;
    pub const SYSCALL_PID: usize = 88;
    pub const USER_CTX_PTR: usize = 96;
    pub const KERNEL_RETURN_CTX: usize = 104;
    pub const USER_RAX_TMP: usize = 168;
    /// Computed rather than a literal: the field is appended after the 64 KiB
    /// embedded kernel stack to keep the asm-critical offsets (`<= 184`)
    /// byte-identical.
    pub const IST_UNSAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, ist_unsafe_sp);
    pub const PANIC_SAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, panic_safe_sp);
    pub const PANIC_UNSAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, panic_unsafe_sp);
    /// Computed, not pinned: no asm outside this module reads it, so it may
    /// move as the tail grows.
    pub const CURRENT_TASK_ID: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, current_task_id);
    /// Computed, not pinned — same rules as [`CURRENT_TASK_ID`].
    pub const CURRENT_TASK_PRIORITY: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, current_task_priority);
}

/// IST/exception safe-stack region bounds used by
/// [`super::super::super::arch::x86_64::naked::__safestack_pointer_address`]
/// to decide, purely from the running `RSP`, whether instrumented code is
/// executing on an IST/exception stack or in task/kernel/boot context.
///
/// The canonical layout lives in `slopos_mm::memory_layout_defs`; it is
/// duplicated here because the SafeStack resolver is in OSTD — below `mm` in
/// the crate graph — and must supply it as a naked-asm `const` operand. A
/// compile-time razor in `memory_layout_defs.rs` asserts the two match.
pub const SAFESTACK_IST_REGION_BASE: u64 = 0xFFFF_FFFF_C000_0000;

/// Span of [`SAFESTACK_IST_REGION_BASE`] (256 MiB: `C000_0000..D000_0000`).
pub const SAFESTACK_IST_REGION_SPAN: u64 = 0x1000_0000;

/// BSP's PCR.
///
/// Exported with a stable symbol name so the `_start` trampoline in
/// `boot/limine_entry.s` can initialise `self_ref`, `unsafe_sp` and `GS_BASE`
/// *before* the first instrumented Rust function runs: every function compiled
/// with `-Zsanitizer=safestack` reads `gs:[0]` in its prologue.
#[unsafe(no_mangle)]
pub static BSP_PCR: SyncUnsafeCell<ProcessorControlRegion> =
    SyncUnsafeCell::new(ProcessorControlRegion::new());

/// Statically-allocated AP PCRs.
///
/// Exported with a stable symbol so the AP bootstrap trampoline in
/// `boot/src/smp.rs` can reach individual entries — though in practice it goes
/// through the [`AP_PCR_PTRS`] lookup table below, because the PCR is ~72 KiB
/// and not a power of two.
#[unsafe(no_mangle)]
pub static AP_PCRS: SyncUnsafeCell<[ProcessorControlRegion; MAX_STATIC_APS]> =
    SyncUnsafeCell::new({
        const INIT: ProcessorControlRegion = ProcessorControlRegion::new();
        [INIT; MAX_STATIC_APS]
    });

/// Lookup table mapping AP slot index to the corresponding `AP_PCRS` entry
/// pointer, populated on the BSP by [`init_ap_pcr_lookup`] before any AP is
/// started. The AP bootstrap trampoline must install GS_BASE before any
/// instrumented Rust can run, and uses this table rather than reimplementing
/// "multiply by sizeof(PCR)" in hand-rolled asm.
///
/// Raw pointers are not `Sync`; the wrapper carries a single-writer-during-boot
/// discipline instead.
#[repr(transparent)]
pub struct PcrPtrLookup(pub [*mut ProcessorControlRegion; MAX_STATIC_APS]);
unsafe impl Sync for PcrPtrLookup {}

#[unsafe(no_mangle)]
pub static AP_PCR_PTRS: SyncUnsafeCell<PcrPtrLookup> =
    SyncUnsafeCell::new(PcrPtrLookup([ptr::null_mut(); MAX_STATIC_APS]));

/// Pre-populate [`AP_PCR_PTRS`] and prime each AP PCR's `self_ref`,
/// `cpu_id` + `current_task` fields so the naked AP trampoline can install
/// GS_BASE and have `__safestack_pointer_address` find a valid
/// bootstrap task on the very first instrumented call of `ap_entry`.
///
/// `cpu_id` is primed here rather than left to [`init_ap_pcr`] because the
/// trampoline installs GS_BASE before that runs, and [`current_cpu_id`] reads
/// the field straight out of the installed PCR: a slot still holding its static
/// zero answers "CPU 0" — the BSP — for every per-CPU lookup the AP makes in
/// between, which is a window that logs.
///
/// Indexed by 0-based AP slot (AP slot i ↔ PCR at `AP_PCRS[i]`);
/// `bootstrap_tasks[i]` names the AP's bootstrap Task stub whose
/// `unsafe_stack_sp` has already been primed.
///
/// The `&BspToken<'brand>` witnesses BSP-only init; must run on the BSP,
/// exactly once, before any AP is started.
pub fn init_ap_pcr_lookup<'brand>(_token: &BspToken<'brand>, bootstrap_tasks: &[*mut ()]) {
    debug_assert!(bootstrap_tasks.len() <= MAX_STATIC_APS);
    // SAFETY: BSP-only init pre-SMP per the token witness; we are the
    // unique writer to `AP_PCR_PTRS` / `AP_PCRS`.
    unsafe {
        let ptrs = &raw mut (*AP_PCR_PTRS.get()).0;
        let pcrs = AP_PCRS.get();
        for (i, task) in bootstrap_tasks.iter().enumerate() {
            let pcr = &raw mut (*pcrs)[i];
            (*pcr).self_ref = pcr;
            // AP slot `i` is CPU `i + 1`; `init_ap_pcr` later writes the same
            // value from the AP itself.
            (*pcr).cpu_id = (i + 1) as u32;
            (*pcr)
                .current_task
                .store(*task, core::sync::atomic::Ordering::Release);
            (*ptrs)[i] = pcr;
        }
    }
}

/// Raw pointers are not `Sync`; the wrapper is sound because all access is
/// guarded by single-writer semantics during boot init.
#[repr(transparent)]
struct PcrPtrArray([*mut ProcessorControlRegion; MAX_CPUS]);
unsafe impl Sync for PcrPtrArray {}

static ALL_PCRS: SyncUnsafeCell<PcrPtrArray> =
    SyncUnsafeCell::new(PcrPtrArray([ptr::null_mut(); MAX_CPUS]));

static PCR_COUNT: AtomicU32 = AtomicU32::new(0);

static PCR_INIT: InitFlag = InitFlag::new();
static GS_BASE_SET: InitFlag = InitFlag::new();

const INVALID_CPU_IDX: u32 = u32::MAX;

static APIC_ID_TO_CPU_IDX: [AtomicU32; MAX_CPUS] = {
    const INIT: AtomicU32 = AtomicU32::new(INVALID_CPU_IDX);
    [INIT; MAX_CPUS]
};

static BSP_APIC_ID: AtomicU32 = AtomicU32::new(0);

fn register_apic_mapping(cpu_id: usize, apic_id: u32) {
    if (apic_id as usize) < MAX_CPUS {
        APIC_ID_TO_CPU_IDX[apic_id as usize].store(cpu_id as u32, Ordering::Release);
    }
}

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

#[inline]
pub fn apic_id_from_cpu_index(cpu_id: usize) -> Option<u32> {
    get_pcr(cpu_id).map(|pcr| pcr.apic_id)
}

#[inline]
pub fn get_bsp_apic_id() -> u32 {
    BSP_APIC_ID.load(Ordering::Acquire)
}

/// Initialize the BSP's PCR: set up data structures, APIC mapping, mark online.
///
/// Does NOT set GS_BASE — call `install()` on the returned PCR for that.
///
/// The `&BspToken<'brand>` witnesses BSP-only init; must be called
/// exactly once during early BSP boot.
pub fn init_bsp_pcr<'brand>(_token: &BspToken<'brand>, apic_id: u32) {
    if !PCR_INIT.init_once() {
        return;
    }

    BSP_APIC_ID.store(apic_id, Ordering::Release);

    let pcr = BSP_PCR.get();

    // SAFETY: BSP-only init witnessed by the token + PCR_INIT one-shot;
    // we are the unique writer to BSP_PCR / ALL_PCRS / PCR_COUNT.
    // `self_ref`, `unsafe_sp` and GS_BASE were already primed by the `_start`
    // asm trampoline; re-writing them here is idempotent.
    unsafe {
        (*pcr).self_ref = pcr;
        (*pcr).cpu_id = 0;
        (*pcr).apic_id = apic_id;
        (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

        (*ALL_PCRS.get()).0[0] = pcr;
        PCR_COUNT.store(1, Ordering::Release);

        register_apic_mapping(0, apic_id);
        (*pcr).online.store(true, Ordering::Release);
    }
}

/// Initialize a PCR for an Application Processor.
///
/// Returns a pointer to the new PCR. Caller must call `init_gdt()` + `install()`.
///
/// The `&ApToken<'brand>` witnesses AP-init; the AP-init InitFlag
/// inside [`crate::sync::run_ap_init`] guarantees exactly one call
/// per AP slot.
pub fn init_ap_pcr<'brand>(
    token: &crate::sync::ApToken<'brand>,
    apic_id: u32,
) -> *mut ProcessorControlRegion {
    use crate::sync::CpuInitWitness;
    let cpu_id = token.cpu_id();
    if cpu_id == 0 || cpu_id >= MAX_CPUS {
        panic!("init_ap_pcr: invalid cpu_id {}", cpu_id);
    }

    if cpu_id > MAX_STATIC_APS {
        panic!("init_ap_pcr: too many APs (max {})", MAX_STATIC_APS);
    }

    // SAFETY: AP-init witnessed by the token; per-AP one-shot via the
    // mint-side InitFlag means no other CPU is racing this slot.
    unsafe {
        let pcr = &raw mut (*AP_PCRS.get())[cpu_id - 1];

        (*pcr).self_ref = pcr;
        (*pcr).cpu_id = cpu_id as u32;
        (*pcr).apic_id = apic_id;
        (*pcr).kernel_rsp = (*pcr).kernel_stack_top();

        (*ALL_PCRS.get()).0[cpu_id] = pcr;

        PCR_COUNT.fetch_max(cpu_id as u32 + 1, Ordering::AcqRel);

        register_apic_mapping(cpu_id, apic_id);

        pcr
    }
}

/// Safe wrapper around the [`init_ap_pcr`] pointer: encapsulates the
/// `init_gdt` + `install` sequence so AP-bringup callers never dereference it.
pub struct ApPcrHandle {
    ptr: *mut ProcessorControlRegion,
}

unsafe impl Send for ApPcrHandle {}

impl ApPcrHandle {
    /// Allocate and prime the AP's PCR slot. [`Self::init_gdt_and_install`]
    /// must then be called before any instrumented Rust observes `gs:[…]`.
    ///
    /// The `&ApToken<'brand>` witnesses AP-init; the underlying
    /// [`init_ap_pcr`] is per-AP one-shot via the AP-init InitFlag.
    pub fn init<'brand>(token: &crate::sync::ApToken<'brand>, apic_id: u32) -> Self {
        let ptr = init_ap_pcr(token, apic_id);
        Self { ptr }
    }

    pub fn init_gdt_and_install(self) {
        // SAFETY: self.ptr was returned by init_ap_pcr above; the
        // referenced PCR lives in BSS and has not been observed by any
        // other CPU yet (single-writer pre-online phase).
        unsafe {
            (*self.ptr).init_gdt();
            (*self.ptr).install();
        }
    }
}

pub fn mark_gs_base_set() {
    GS_BASE_SET.init_once();
}

/// Read the current CPU index via GS segment.
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

// Migration-atomic accessors for per-CPU PCR scalars (the Linux `this_cpu_*`
// discipline).
//
// `current_pcr` materialises the PCR *pointer* and lets the caller dereference
// it later; in preemptible context an IRQ-driven reschedule can migrate the
// task between the fetch and the access, landing it on the previous CPU's PCR.
// For the preempt count that split is fatal: the increment strands the old CPU
// at a non-zero count while the new CPU's count stays 0, so the matching guard
// drop underflows. Each accessor below is a SINGLE gs-relative instruction,
// which the CPU executes atomically with respect to interrupts.
//
// No `lock` prefix on `add`/`xadd`: these fields are written only by their
// owning CPU, and cross-CPU readers (diagnostics) tolerate stale snapshots.
// x86-TSO plus the `asm!` blocks' implicit compiler barrier supply the rest.
//
// GS_BASE must point at a valid PCR when any of these run — the same
// precondition `current_pcr` documents.

#[inline(always)]
pub(crate) fn preempt_count_inc() {
    // SAFETY: single gs-relative RMW on this CPU's PCR field; GS_BASE
    // is installed by the entry trampolines before any caller runs.
    unsafe {
        core::arch::asm!(
            "add dword ptr gs:[{off}], 1",
            off = const offsets::PREEMPT_COUNT,
            options(nostack),
        );
    }
}

#[inline(always)]
pub(crate) fn preempt_count_dec_fetch_prev() -> u32 {
    let prev: u32;
    // SAFETY: single gs-relative RMW on this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "xadd dword ptr gs:[{off}], {prev:e}",
            off = const offsets::PREEMPT_COUNT,
            prev = inout(reg) u32::MAX => prev,
            options(nostack),
        );
    }
    prev
}

#[inline(always)]
pub(crate) fn preempt_count_get() -> u32 {
    let count: u32;
    // SAFETY: single gs-relative load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {count:e}, gs:[{off}]",
            off = const offsets::PREEMPT_COUNT,
            count = out(reg) count,
            options(nostack, preserves_flags, readonly),
        );
    }
    count
}

/// Only the context-switch count swap may use this, with IRQs disabled.
#[inline(always)]
pub(crate) fn preempt_count_set(count: u32) {
    // SAFETY: single gs-relative store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{off}], {count:e}",
            off = const offsets::PREEMPT_COUNT,
            count = in(reg) count,
            options(nostack, preserves_flags),
        );
    }
}

#[inline(always)]
pub(crate) fn reschedule_pending_set() {
    // SAFETY: single gs-relative store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov dword ptr gs:[{off}], 1",
            off = const offsets::RESCHEDULE_PENDING,
            options(nostack, preserves_flags),
        );
    }
}

#[inline(always)]
pub(crate) fn reschedule_pending_clear() {
    // SAFETY: single gs-relative store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov dword ptr gs:[{off}], 0",
            off = const offsets::RESCHEDULE_PENDING,
            options(nostack, preserves_flags),
        );
    }
}

#[inline(always)]
pub(crate) fn reschedule_pending_get() -> u32 {
    let pending: u32;
    // SAFETY: single gs-relative load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {pending:e}, gs:[{off}]",
            off = const offsets::RESCHEDULE_PENDING,
            pending = out(reg) pending,
            options(nostack, preserves_flags, readonly),
        );
    }
    pending
}

/// `xchg` is implicitly locked, so a same-instant set from a local IRQ is
/// either observed or preserved, never lost.
#[inline(always)]
pub(crate) fn reschedule_pending_take() -> u32 {
    let pending: u32;
    // SAFETY: single gs-relative RMW on this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "xchg dword ptr gs:[{off}], {pending:e}",
            off = const offsets::RESCHEDULE_PENDING,
            pending = inout(reg) 0u32 => pending,
            options(nostack, preserves_flags),
        );
    }
    pending
}

/// Returns `None` if `cpu_id` is invalid or the PCR has not been initialized.
pub fn get_pcr(cpu_id: usize) -> Option<&'static ProcessorControlRegion> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    unsafe {
        let ptr = (*ALL_PCRS.get()).0[cpu_id];
        if ptr.is_null() { None } else { Some(&*ptr) }
    }
}

/// # Safety
/// Caller must ensure exclusive access to the PCR.
pub unsafe fn get_pcr_mut(cpu_id: usize) -> Option<&'static mut ProcessorControlRegion> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    let ptr = (*ALL_PCRS.get()).0[cpu_id];
    if ptr.is_null() { None } else { Some(&mut *ptr) }
}

/// `get_pcr_mut` exposed as a safe surface for per-CPU init-only mutators (TSS
/// rsp0 update, IST slot binding), race-free under Inv. 8: each CPU mutates
/// only its own `cpu_id` slot. Out-of-range `cpu_id` returns `None`.
pub fn get_pcr_mut_via_token(cpu_id: usize) -> Option<&'static mut ProcessorControlRegion> {
    // SAFETY: per-CPU slot mutation under Inv. 8 — callers commit to
    // writing only `cpu_id`'s slot. The PCR is alive for the kernel
    // lifetime once initialised, so the `'static` reborrow is sound.
    unsafe { get_pcr_mut(cpu_id) }
}

/// Safe surface for "get the *local* CPU's PCR by id", for the common pattern
/// of reading this CPU's PCR fields where the GS-relative fast path is not
/// required. Returns `None` if PCR init hasn't run yet.
#[inline]
pub fn current_pcr_local() -> Option<&'static ProcessorControlRegion> {
    get_pcr(get_current_cpu())
}

/// Mutable variant of [`current_pcr_local`]. Sound under Inv. 8: the
/// caller commits to mutating only this CPU's slot.
#[inline]
pub fn current_pcr_local_mut() -> Option<&'static mut ProcessorControlRegion> {
    get_pcr_mut_via_token(get_current_cpu())
}

/// Prime/re-prime the current CPU's IST/exception SafeStack data-stack
/// pointer ([`ProcessorControlRegion::ist_unsafe_sp`]).
///
/// Single-writer-per-CPU by construction: called at boot by `ist_stacks` with
/// interrupts disabled, and by `retire_faulted_cpu` — the one exception path
/// that `schedule()`s away without unwinding the IST data stack, so the
/// abandoned depth does not accumulate. The only other writer is the LLVM
/// SafeStack prologue, which runs on this same CPU.
#[inline]
pub fn set_local_ist_unsafe_sp(top: u64) {
    if let Some(pcr) = current_pcr_local() {
        // SAFETY: `ist_unsafe_sp` is a per-CPU slot; this writes only the
        // owning CPU's PCR (Inv. 8). The cell's inner `u64` is the same
        // location the SafeStack sanitizer reads/writes on this CPU.
        unsafe {
            *pcr.ist_unsafe_sp.get() = top;
        }
    }
}

/// Diagnostics only; the hot path reads this slot via the naked
/// `__safestack_pointer_address` asm, never this fn.
#[inline]
pub fn local_ist_unsafe_sp() -> u64 {
    current_pcr_local()
        // SAFETY: per-CPU read of this CPU's slot; benign for diagnostics.
        .map(|pcr| unsafe { *pcr.ist_unsafe_sp.get() })
        .unwrap_or(0)
}

/// Called by `ist_stacks` during per-CPU bringup, before any fatal fault can
/// occur.
#[inline]
pub fn set_local_panic_safe_sp(top: u64) {
    if let Some(pcr) = current_pcr_local() {
        // SAFETY: per-CPU slot write of the owning CPU's PCR (Inv. 8).
        unsafe {
            *pcr.panic_safe_sp.get() = top;
        }
    }
}

#[inline]
pub fn set_local_panic_unsafe_sp(top: u64) {
    if let Some(pcr) = current_pcr_local() {
        // SAFETY: per-CPU slot write of the owning CPU's PCR (Inv. 8).
        unsafe {
            *pcr.panic_unsafe_sp.get() = top;
        }
    }
}

/// Diagnostics / tests only.
#[inline]
pub fn local_panic_safe_sp() -> u64 {
    current_pcr_local()
        .map(|pcr| unsafe { *pcr.panic_safe_sp.get() })
        .unwrap_or(0)
}

/// Diagnostics / tests only.
#[inline]
pub fn local_panic_unsafe_sp() -> u64 {
    current_pcr_local()
        .map(|pcr| unsafe { *pcr.panic_unsafe_sp.get() })
        .unwrap_or(0)
}

fn counter_exit_saturating(counter: &AtomicU32) -> u32 {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return 0;
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::SeqCst,
            Ordering::Acquire,
        ) {
            Ok(_) => return current,
            Err(next) => current = next,
        }
    }
}

/// Returns the PREVIOUS depth: non-zero means the fatal path itself faulted and
/// the caller must degrade to the format-free abort. Never decremented — a CPU
/// that enters the fatal path does not leave it.
#[inline]
pub fn panic_depth_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_depth.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Returns the previous in-flight depth: non-zero means the panic handler
/// re-entered before the prior panic reached an unwind catch boundary or a
/// fatal halt.
#[inline]
pub fn panic_in_flight_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_in_flight.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Call only after a caught panic crosses an unwind catch boundary. Returns the
/// previous in-flight depth and saturates at zero.
#[inline]
pub fn panic_in_flight_exit() -> u32 {
    current_pcr_local()
        .map(|pcr| counter_exit_saturating(&pcr.panic_in_flight))
        .unwrap_or(0)
}

#[inline]
pub fn panic_in_flight_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_in_flight.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Context-switch use only: an unwinding task runs interrupts-on and can
/// migrate, so the depth must travel with the task or the enter-CPU's counter
/// leaks and a later `AbortOnUnwind` drop on that CPU false-aborts.
#[inline]
pub fn panic_in_flight_store(depth: u32) {
    if let Some(pcr) = current_pcr_local() {
        pcr.panic_in_flight.store(depth, Ordering::Release);
    }
}

/// Returns the previous nesting depth.
#[inline]
pub fn interrupt_nesting_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| {
            pcr.in_interrupt.store(true, Ordering::SeqCst);
            pcr.interrupt_nesting.fetch_add(1, Ordering::SeqCst)
        })
        .unwrap_or(0)
}

/// Returns the previous nesting depth and saturates at zero.
#[inline]
pub fn interrupt_nesting_exit() -> u32 {
    current_pcr_local()
        .map(|pcr| {
            let previous = counter_exit_saturating(&pcr.interrupt_nesting);
            if previous <= 1 {
                pcr.in_interrupt.store(false, Ordering::SeqCst);
            }
            previous
        })
        .unwrap_or(0)
}

#[inline]
pub fn interrupt_nesting_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.interrupt_nesting.load(Ordering::Acquire))
        .unwrap_or(0)
}

#[inline]
pub fn in_interrupt_context() -> bool {
    current_pcr_local()
        .map(|pcr| {
            pcr.interrupt_nesting.load(Ordering::Acquire) != 0
                || pcr.in_interrupt.load(Ordering::Acquire)
        })
        .unwrap_or(false)
}

/// Must stay one instruction: no table lookup, no bounds check, nothing that
/// could take a lock.
#[inline(always)]
pub fn bh_pending_set() {
    // SAFETY: single gs-relative store to this CPU's PCR field; GS_BASE is
    // installed by the entry trampolines before any caller runs.
    unsafe {
        core::arch::asm!(
            "mov byte ptr gs:[{off}], 1",
            off = const offsets::BH_PENDING,
            options(nostack, preserves_flags),
        );
    }
}

/// One instruction rather than a call through the PCR table: it sits on every
/// outermost unlock, and a call would inflate the frame of every function that
/// releases a lock.
#[inline(always)]
pub fn bh_pending_get() -> bool {
    let pending: u8;
    // SAFETY: single gs-relative load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {pending}, byte ptr gs:[{off}]",
            off = const offsets::BH_PENDING,
            pending = out(reg_byte) pending,
            options(nostack, preserves_flags, readonly),
        );
    }
    pending != 0
}

/// Clear the flag and report whether it was set.
#[inline]
pub fn bh_pending_take() -> bool {
    current_pcr_local().is_some_and(|pcr| pcr.bh_pending.swap(false, Ordering::AcqRel))
}

/// Claim the drain for this CPU; returns whether it was already claimed.
#[inline]
pub fn bh_active_swap(active: bool) -> bool {
    current_pcr_local().is_some_and(|pcr| pcr.bh_active.swap(active, Ordering::AcqRel))
}

#[inline]
pub fn bh_active_clear() {
    if let Some(pcr) = current_pcr_local() {
        pcr.bh_active.store(false, Ordering::Release);
    }
}

/// Returns the previous recovery depth.
#[inline]
pub fn recovery_depth_enter() -> u32 {
    recovery_depth_enter_for_cpu(get_current_cpu())
}

/// Returns the previous recovery depth and saturates at zero.
#[inline]
pub fn recovery_depth_exit() -> u32 {
    recovery_depth_exit_for_cpu(get_current_cpu())
}

#[inline]
pub fn recovery_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.recovery_depth.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Cross-CPU read; `0` for an unknown or offline CPU. Non-zero means that CPU
/// is inside a `run_recoverable` scope, where a caught panic may run the DWARF
/// unwinder with interrupts disabled — and thus without advancing its per-CPU
/// timer tick — for a legitimately long time, which is the grace the NMI
/// watchdog grants it before treating it as stuck.
#[inline]
pub fn recovery_depth_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| pcr.recovery_depth.load(Ordering::Acquire))
        .unwrap_or(0)
}

#[inline]
pub fn recovery_depth_store(depth: u32) {
    if let Some(pcr) = current_pcr_local() {
        pcr.recovery_depth.store(depth, Ordering::Release);
    }
}

/// Explicit `cpu_id` so a recovery guard's Drop exits the same per-CPU slot it
/// entered.
#[doc(hidden)]
#[inline]
pub fn recovery_depth_enter_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| pcr.recovery_depth.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Explicit `cpu_id` so a recovery guard's Drop exits the same per-CPU slot it
/// entered.
#[doc(hidden)]
#[inline]
pub fn recovery_depth_exit_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| counter_exit_saturating(&pcr.recovery_depth))
        .unwrap_or(0)
}

/// Raw store of `top` into this CPU's `PCR.ist_unsafe_sp`, bypassing the
/// SafeStack sanitizer, to re-prime the exception data-stack pointer when a
/// handler abandons it without unwinding (`retire_faulted_cpu`).
///
/// Naked because that abandoning code runs *on* an IST stack, so
/// `__safestack_pointer_address` resolves THIS very slot as its own data-SP: an
/// instrumented setter's SafeStack epilogue would restore the slot to its entry
/// value and undo the re-prime. Must therefore be called DIRECTLY — an
/// instrumented wrapper re-introduces the same clobber. Clobbers only `rax`.
///
/// [`set_local_ist_unsafe_sp`] remains correct for *boot* priming, which runs
/// on the boot/kernel stack where the resolver picks the per-task slot instead.
#[unsafe(naked)]
pub extern "sysv64" fn reset_ist_unsafe_sp(top: u64) {
    naked_asm!(
        // rax = PCR base (self_ref @ offset 0, == this CPU's gs base).
        "mov rax, gs:[{off_self_ref}]",
        // PCR.ist_unsafe_sp = top  (top is arg0 = rdi).
        "mov [rax + {off_ist_sp}], rdi",
        "ret",
        off_self_ref = const offsets::SELF_REF,
        off_ist_sp = const offsets::IST_UNSAFE_SP,
    )
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

/// Record progress on this CPU. See [`ProcessorControlRegion::heartbeat`].
#[inline]
pub fn heartbeat_bump() {
    if let Some(pcr) = current_pcr_local() {
        pcr.heartbeat.fetch_add(1, Ordering::Relaxed);
    }
}

/// Read `cpu_id`'s progress counter; `0` for an unknown CPU.
#[inline]
pub fn heartbeat_for_cpu(cpu_id: usize) -> u64 {
    get_pcr(cpu_id)
        .map(|pcr| pcr.heartbeat.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Publish whether this CPU's LAPIC timer is delivering periodic ticks,
/// which is what makes it eligible to be watched.
#[inline]
pub fn set_timer_armed(armed: bool) {
    if let Some(pcr) = current_pcr_local() {
        pcr.timer_armed.store(armed, Ordering::Release);
    }
}

/// Whether `cpu_id` has a running periodic timer.
#[inline]
pub fn timer_is_armed(cpu_id: usize) -> bool {
    get_pcr(cpu_id)
        .map(|pcr| pcr.timer_armed.load(Ordering::Acquire))
        .unwrap_or(false)
}

/// Set or clear this CPU's watchdog suppression, returning the old value.
#[inline]
pub fn set_watchdog_suppressed(suppressed: bool) -> bool {
    current_pcr_local()
        .map(|pcr| pcr.watchdog_suppressed.swap(suppressed, Ordering::AcqRel))
        .unwrap_or(false)
}

/// Whether `cpu_id` has asked not to be watched.
#[inline]
pub fn watchdog_is_suppressed(cpu_id: usize) -> bool {
    get_pcr(cpu_id)
        .map(|pcr| pcr.watchdog_suppressed.load(Ordering::Acquire))
        .unwrap_or(false)
}

// ==================== PER-CPU DATA ACCESSORS ====================
// These operate on the *current* CPU's PCR via GS_BASE.

/// Set the current task pointer and its id for this CPU.
///
/// Two gs-relative stores, each migration-atomic (see the
/// single-instruction per-CPU ops block above) — neither value can land in a
/// PCR the writing task has been migrated away from.
///
/// The id and the priority travel with the pointer rather than in their own
/// setters so the three cannot be written independently: [`current_task_id`]
/// and [`current_task_priority_for`] are only trustworthy because every
/// publisher of the pointer necessarily publishes both in the same call. Pass
/// `u32::MAX` (`INVALID_TASK_ID`) and [`PRIORITY_NONE`] for a task with no
/// registry identity, such as a pre-heap bootstrap stub.
#[inline]
pub(crate) fn set_current_task(task: *mut (), task_id: u32, priority: u8) {
    if !GS_BASE_SET.is_set() {
        return;
    }
    // Retire the old id before the pointer moves. Readers discriminate on the
    // id — `CurrentTask::get` treats `INVALID_TASK_ID` as "no task, and
    // therefore possibly a bootstrap stub" — so publishing the pointer first
    // would briefly pair a new pointer with the *previous* id. When the new
    // pointer is a stub and the previous id was a real task, that window reads
    // as "a live task lives at this stub address".
    set_current_task_id(slopos_abi::task::INVALID_TASK_ID);
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{off}], {task}",
            off = const offsets::CURRENT_TASK,
            task = in(reg) task,
            options(nostack, preserves_flags),
        );
    }
    set_current_task_id(task_id);
    set_current_task_priority(priority);
}

/// Publish `task` as the task running on this CPU.
///
/// The typed counterpart of the reader in [`crate::task::cell`]: this is where
/// the monomorphisation the PCR slot holds is *decided*, and the
/// `PcrTaskType` bound is what makes reader and writer name the same one. The
/// erased [`set_current_task`] is `pub(crate)`, so this and
/// [`park_bootstrap_task`] are the only ways in from outside OSTD.
#[inline]
pub fn set_current_task_typed<K, U>(
    task: *mut crate::task::kernel_task::TaskInner<K, U>,
    task_id: u32,
    priority: u8,
) where
    crate::task::kernel_task::TaskInner<K, U>: crate::task::PcrTaskType,
{
    set_current_task(task.cast::<()>(), task_id, priority);
}

/// Park this CPU on a pre-heap bootstrap stub.
///
/// A stub is deliberately *not* a `TaskInner`: it holds only the eight-byte
/// `unsafe_stack_sp` prefix an instrumented prologue reads. Publishing
/// `INVALID_TASK_ID` and [`PRIORITY_NONE`] alongside it is what every reader's
/// stub filter keys on, so the two travel together here rather than being
/// spelled out at each call site.
#[inline]
pub fn park_bootstrap_task(stub: *mut ()) {
    set_current_task(stub, slopos_abi::task::INVALID_TASK_ID, PRIORITY_NONE);
}

/// Get the current task pointer for this CPU.
///
/// Single gs-relative load: a preemptible caller that is migrated
/// mid-call still reads the PCR of the CPU executing the instruction
/// — which, post-migration, holds *this* task again — never a stale
/// pointer into the previous CPU's PCR.
#[inline]
pub fn get_current_task() -> *mut () {
    if !GS_BASE_SET.is_set() {
        return ptr::null_mut();
    }
    let task: *mut ();
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {task}, gs:[{off}]",
            off = const offsets::CURRENT_TASK,
            task = out(reg) task,
            options(nostack, preserves_flags, readonly),
        );
    }
    task
}

/// Move one outgoing-task ownership reference into this CPU's deferred slot.
///
/// Returns `Err(task)` if the previous reference has not yet been taken. The
/// caller must not release a successfully installed reference by any other
/// path.
#[inline]
pub fn defer_previous_task(task: *mut ()) -> Result<(), *mut ()> {
    if task.is_null() || !GS_BASE_SET.is_set() {
        return Err(task);
    }
    // SAFETY: GS_BASE is installed (checked above), and only the running CPU
    // mutates its own deferred slot.
    let slot = unsafe { &current_pcr().previous_task };
    slot.compare_exchange(ptr::null_mut(), task, Ordering::Release, Ordering::Relaxed)
        .map(|_| ())
        .map_err(|_| task)
}

/// Take this CPU's deferred outgoing-task ownership reference, if present.
/// The returned reference must be released exactly once by the caller.
#[inline]
pub fn take_previous_task() -> *mut () {
    if !GS_BASE_SET.is_set() {
        return ptr::null_mut();
    }
    // SAFETY: GS_BASE is installed (checked above), and only the running CPU
    // drains its own deferred slot.
    unsafe {
        current_pcr()
            .previous_task
            .swap(ptr::null_mut(), Ordering::Acquire)
    }
}

/// Read the current CPU's `syscall_pid` atomically. Returns
/// `u32::MAX` (`INVALID_PROCESS_ID` sentinel) if GS_BASE has not yet
/// been installed on this CPU. Folds the single `unsafe` deref of the
/// PCR pointer interior to OSTD so kernel-half copy / mm helpers stay
/// in safe Rust. Single gs-relative load (migration-atomic): user
/// pointer validation must see the pid the *executing* CPU's
/// dispatcher installed, never a stale neighbour-CPU value.
#[inline]
pub fn current_syscall_pid() -> u32 {
    if !GS_BASE_SET.is_set() {
        return u32::MAX;
    }
    let pid: u32;
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {pid:e}, gs:[{off}]",
            off = const offsets::SYSCALL_PID,
            pid = out(reg) pid,
            options(nostack, preserves_flags, readonly),
        );
    }
    pid
}

/// Store `pid` into the current CPU's `syscall_pid`. No-op until the
/// PCR is installed. Counterpart of [`current_syscall_pid`].
/// Single gs-relative store (migration-atomic).
#[inline]
pub fn set_current_syscall_pid(pid: u32) {
    if !GS_BASE_SET.is_set() {
        return;
    }
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{off}], {pid:e}",
            off = const offsets::SYSCALL_PID,
            pid = in(reg) pid,
            options(nostack, preserves_flags),
        );
    }
}

/// Read the id of the task running on this CPU, or `u32::MAX`
/// (`INVALID_TASK_ID`) before this CPU's first dispatch or until GS_BASE is
/// installed.
///
/// Single gs-relative load, so it is migration-atomic on the same argument as
/// [`current_syscall_pid`]: a caller preempted mid-call still reads the PCR of
/// the CPU executing the instruction, which post-migration holds this task
/// again. Answers "which task am I" without dereferencing the task, which is
/// what makes it safe while `current_task` names a pre-heap bootstrap stub.
#[inline]
pub fn current_task_id() -> u32 {
    if !GS_BASE_SET.is_set() {
        return u32::MAX;
    }
    let id: u32;
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // load from this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov {id:e}, gs:[{off}]",
            off = const offsets::CURRENT_TASK_ID,
            id = out(reg) id,
            options(nostack, preserves_flags, readonly),
        );
    }
    id
}

/// Publish the id of the task this CPU is switching to.
///
/// Private on purpose: [`set_current_task`] is the only caller, which is what
/// keeps the id and the pointer from ever naming different tasks.
/// Single gs-relative store (migration-atomic).
#[inline]
fn set_current_task_id(task_id: u32) {
    if !GS_BASE_SET.is_set() {
        return;
    }
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{off}], {id:e}",
            off = const offsets::CURRENT_TASK_ID,
            id = in(reg) task_id,
            options(nostack, preserves_flags),
        );
    }
}

/// Read the id of the task running on `cpu_id`, or `u32::MAX` when that CPU has
/// no PCR or has not dispatched yet. Cross-CPU sibling of [`current_task_id`].
#[inline]
pub fn current_task_id_for(cpu_id: usize) -> u32 {
    get_pcr(cpu_id).map_or(u32::MAX, |pcr| pcr.current_task_id.load(Ordering::Acquire))
}

/// Publish the priority of the task this CPU is switching to.
///
/// Private for the same reason as [`set_current_task_id`]: keeping
/// [`set_current_task`] the only writer is what stops the priority from ever
/// describing a task other than the one the pointer names.
#[inline]
fn set_current_task_priority(priority: u8) {
    if !GS_BASE_SET.is_set() {
        return;
    }
    // SAFETY: GS_BASE is installed (checked above); single gs-relative
    // store to this CPU's PCR field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{off}], {prio}",
            off = const offsets::CURRENT_TASK_PRIORITY,
            prio = in(reg_byte) priority,
            options(nostack, preserves_flags),
        );
    }
}

/// Read the scheduling priority of the task running on `cpu_id`, or
/// [`PRIORITY_NONE`] when that CPU has no PCR or is running nothing.
///
/// The whole point is that this answers the preemption question **without
/// dereferencing a foreign CPU's task**: that CPU's switch tail may be
/// releasing the outgoing dispatch reference and running the task's
/// allocator-heavy destructor concurrently.
#[inline]
pub fn current_task_priority_for(cpu_id: usize) -> u8 {
    get_pcr(cpu_id).map_or(PRIORITY_NONE, |pcr| {
        pcr.current_task_priority.load(Ordering::Acquire)
    })
}

/// Set the idle-task pointer for `cpu_id`.
///
/// Cross-CPU-safe (takes explicit `cpu_id`) so the BSP can seed every
/// AP's idle slot during bring-up before that AP's GS_BASE is
/// installed.  Single-writer discipline: each idle task is installed
/// exactly once per CPU, at `create_idle_task_for_cpu()` time.
#[inline]
pub fn set_idle_task(cpu_id: usize, task: *mut ()) {
    if let Some(pcr) = get_pcr(cpu_id) {
        pcr.idle_task.store(task, Ordering::Release);
    }
}

/// Get the idle-task pointer for `cpu_id`.  Returns null for
/// uninitialised / out-of-range CPUs.
#[inline]
pub fn get_idle_task(cpu_id: usize) -> *mut () {
    get_pcr(cpu_id)
        .map(|pcr| pcr.idle_task.load(Ordering::Acquire))
        .unwrap_or(ptr::null_mut())
}

/// Get the current-task pointer for `cpu_id`.  Cross-CPU variant of
/// [`get_current_task`]; used by the scheduler when it needs to read
/// another CPU's running task (e.g. remote-wakeup fast paths).
#[inline]
pub fn get_current_task_for(cpu_id: usize) -> *mut () {
    get_pcr(cpu_id)
        .map(|pcr| pcr.current_task.load(Ordering::Acquire))
        .unwrap_or(ptr::null_mut())
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

/// Register the IPI send function from the APIC driver. The
/// `&BspToken<'brand>` witnesses BSP-only init.
pub fn register_send_ipi_to_cpu_fn<'brand>(_token: &BspToken<'brand>, f: SendIpiToCpuFn) {
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

/// Register the LAPIC ID reader function from the APIC driver. The
/// `&BspToken<'brand>` witnesses BSP-only init.
pub fn register_lapic_id_fn<'brand>(_token: &BspToken<'brand>, f: fn() -> u32) {
    LAPIC_ID_FN.store(f as *mut (), Ordering::Release);
}

// ==================== NMI IPI ====================

pub type SendNmiToCpuFn = fn(u32);

static SEND_NMI_TO_CPU_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Register the NMI send function from the APIC driver. The
/// `&BspToken<'brand>` witnesses BSP-only init.
pub fn register_send_nmi_fn<'brand>(_token: &BspToken<'brand>, f: SendNmiToCpuFn) {
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

pub type SendNmiBroadcastFn = fn();

static SEND_NMI_BROADCAST_FN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Register the broadcast-NMI send function from the APIC driver (BSP-only).
pub fn register_send_nmi_broadcast_fn<'brand>(_token: &BspToken<'brand>, f: SendNmiBroadcastFn) {
    SEND_NMI_BROADCAST_FN.store(f as *mut (), Ordering::Release);
}

/// Broadcast an NMI to all OTHER CPUs (Reliable Abort Core stop-the-world).
///
/// No-op if the send function has not been registered yet (e.g. a panic before
/// APIC init — the single-CPU early-boot case where there are no peers to stop).
pub fn send_nmi_broadcast() {
    let fn_ptr = SEND_NMI_BROADCAST_FN.load(Ordering::Acquire);
    if !fn_ptr.is_null() {
        let f: SendNmiBroadcastFn = unsafe { core::mem::transmute(fn_ptr) };
        f();
    }
}
