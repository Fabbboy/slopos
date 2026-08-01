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
//! - `slopos-ostd/src/user/asm/user_return.s` (`__ostd_user_return`)
//! - `core/context_switch.s` (context_switch_user)

use core::arch::naked_asm;
use core::cell::SyncUnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::arch::x86_64::gdt::{GdtLayout, SegmentSelector, Tss64};
use crate::arch::x86_64::msr::Msr;

use crate::sync::BspToken;
use crate::sync::init_flag::InitFlag;
use crate::user::context::UserContext;

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
///   - slopos-ostd/src/user/asm/user_return.s (`__ostd_user_return`)
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
    ///
    /// Written by the scheduler's `dispatch()` helper every context
    /// switch.  Read by the SafeStack sanitizer's naked
    /// `__safestack_pointer_address()` on every instrumented function
    /// prologue via `gs:[CURRENT_TASK]`.  Must always point at a valid
    /// Task (or bootstrap stub) with a primed `unsafe_stack_sp`
    /// whenever instrumented code may run; nulling it crashes the next
    /// instrumented prologue.
    pub current_task: AtomicPtr<()>, // offset 40

    /// Pointer to this CPU's idle task (opaque).
    ///
    /// Written once per CPU by `install_idle_task()` during
    /// `create_idle_task_for_cpu()`; read by the scheduler's
    /// idle-stack resolve and `run_ready_task_from_idle` dispatch
    /// paths.
    pub idle_task: AtomicPtr<()>, // offset 48

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

    // ==================== USER-MODE ROUND-TRIP SLOTS ====================
    // Read+written by the `__ostd_user_return` trampoline asm via
    // `gs:[…]`.  Their offsets are pinned by the const-asserts below.
    /// Per-CPU active `UserContext` pointer.  Set by
    /// `PcrUserModeBackend::execute_round_trip` before iretq into user
    /// mode; consumed by `__ostd_user_return` to write user state back.
    pub user_ctx_ptr: AtomicPtr<UserContext>, // offset 96

    /// Saved kernel callee-save snapshot used by `__ostd_user_return`
    /// to unwind back to the caller of `execute_round_trip`.
    pub kernel_return_ctx: SyncUnsafeCell<KernelReturnContext>, // offset 104

    /// Per-CPU scratch slot for the user RAX value during the
    /// `__ostd_user_return` trampoline.  Spilling RAX onto the kernel
    /// stack at `kernel_rsp - 8` would collide with the next CPU-pushed
    /// IRET frame's SS slot at TSS.RSP0; this per-CPU slot is the same
    /// fix Asterinas / Linux apply to their SYSCALL fast paths.
    pub user_rax_tmp: SyncUnsafeCell<u64>, // offset 168

    // ==================== EMBEDDED GDT ====================
    /// Per-CPU Global Descriptor Table.
    /// Contains kernel/user code/data segments + TSS descriptor.
    pub gdt: GdtLayout, // offset 192

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

    /// Per-CPU SafeStack **data**-stack pointer for IST/exception context.
    ///
    /// `__safestack_pointer_address` returns the address of THIS slot
    /// (instead of `current_task->unsafe_stack_sp`) whenever the running
    /// `RSP` is inside `EXCEPTION_STACK_REGION`, so instrumented code in
    /// an exception handler (`klog!`/`panic!`/`core::fmt`) walks a
    /// dedicated, mapped, guard-paged per-CPU exception data stack rather
    /// than the interrupted task's small data stack.  Primed to the
    /// exception data-stack top by `ist_stacks` before interrupts are
    /// enabled, self-balancing across nested faults, and re-primed by
    /// `retire_faulted_cpu` (the one exception path that abandons without
    /// unwinding).  Appended last so every asm-critical PCR offset
    /// (`<= 184`) and the embedded GDT/TSS layout stay byte-identical.
    ///
    /// `SyncUnsafeCell` (like `user_rax_tmp`) gives interior mutability so
    /// `__safestack_pointer_address` can hand its raw `u64` address to the
    /// LLVM SafeStack prologue while Rust primes/re-primes it through the
    /// owning CPU.  The inner `u64` sits at offset 0 of the cell, so
    /// `offset_of!(PCR, ist_unsafe_sp)` is the address the asm returns.
    pub ist_unsafe_sp: SyncUnsafeCell<u64>,

    /// Reliable Abort Core — per-CPU emergency SAFE-stack top (RSP).
    ///
    /// The fatal-fault trampoline switches `RSP` here before any panic
    /// formatting, so a panic from a deep/near-full safe stack still has
    /// headroom. Primed by `ist_stacks` alongside `ist_unsafe_sp`. Appended
    /// after `ist_unsafe_sp` so every asm-critical offset (`<= 184`) stays
    /// byte-identical.
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
    /// (now-suspect) report. Mirrors Linux `die_nest_count` / Asterinas
    /// `IN_PANIC`.
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
    /// after leaving the IRQ-off switch window. Kept opaque here because the
    /// concrete task type belongs to the scheduler crate.
    pub previous_task: AtomicPtr<()>,

    /// Panic-recovery nesting depth for this CPU: nested catch scopes unwind
    /// one level at a time, and only the CPU that entered recovery observes
    /// it as active.
    pub recovery_depth: AtomicU32,

    /// ID of the task `current_task` names, republished by the same
    /// `dispatch()` that writes the pointer.
    ///
    /// Exists so the many callers that want only "who am I" never dereference
    /// the task at all. Reading the id out of the pointer costs a load through
    /// a pointer that may name a pre-heap bootstrap stub, and it is the one
    /// question that does not need a borrow. `INVALID_TASK_ID` until this CPU
    /// dispatches for the first time.
    ///
    /// Appended at the tail: every asm-critical offset (`<= 184`) stays
    /// byte-identical, and a 4-byte tail field cannot perturb the 4096-byte
    /// alignment the razors pin.
    pub current_task_id: AtomicU32,

    /// Scheduling priority of the task `current_task` names, republished by the
    /// same [`set_current_task`] that writes the pointer and the id.
    ///
    /// Exists so a wake publisher can ask "would the newcomer preempt what is
    /// running over there?" without dereferencing a *foreign* CPU's task. That
    /// dereference raced the target CPU's switch tail, which reclaims and
    /// releases the outgoing dispatch reference and can run the allocator-heavy
    /// destructor — so the read could land in freed memory.
    ///
    /// [`PRIORITY_NONE`] until this CPU dispatches for the first time. After
    /// that it always names a real task: a CPU with nothing to run parks on its
    /// idle task, which publishes `TaskPriority::Idle` rather than the
    /// sentinel. Both lose to every other priority, which is what the
    /// comparison needs — the sentinel is for "this CPU has never dispatched",
    /// not for "this CPU is idle".
    pub current_task_priority: AtomicU8,

    /// Monotonic progress counter for the lockup detector.
    ///
    /// Bumped from the timer tick before any lock is taken, and from
    /// [`crate::watchdog::touch`] inside the few bounded loops that run
    /// long enough to outlast a tick. A watcher compares it against its own
    /// previous reading, so the detector does no clock arithmetic and
    /// cannot be fooled by emulation or host steal time.
    ///
    /// Appended at the tail with the rest: every asm-critical offset
    /// (`<= 184`) stays byte-identical and the 4096-byte alignment the
    /// razors pin is untouched.
    pub heartbeat: AtomicU64,

    /// Whether this CPU's LAPIC timer is running periodically.
    ///
    /// A CPU is marked online before it starts its timer — APs boot ahead
    /// of calibration and start theirs from the scheduler loop — so
    /// `online` alone would have the detector watch a CPU that cannot yet
    /// tick. A zero heartbeat cannot stand in for this: [`crate::watchdog::touch`]
    /// makes it non-zero without a timer ever having fired.
    pub timer_armed: AtomicBool,

    /// Set while this CPU is deliberately running without timer ticks, by
    /// a [`crate::watchdog::Suppress`] token.
    pub watchdog_suppressed: AtomicBool,
}

/// `current_task_priority` for a CPU that is running nothing schedulable.
///
/// Numerically worst, because `TaskPriority` orders `High = 0` upward: any real
/// task outranks it, which reproduces the old "current pointer was null, so the
/// newcomer wins" branch without a pointer.
pub const PRIORITY_NONE: u8 = u8::MAX;

// Compile-time offset verification.
const _: () = {
    assert!(core::mem::offset_of!(ProcessorControlRegion, self_ref) == 0);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rsp_tmp) == 8);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_rsp) == 16);
    assert!(core::mem::offset_of!(ProcessorControlRegion, cpu_id) == 24);
    assert!(core::mem::offset_of!(ProcessorControlRegion, apic_id) == 28);
    assert!(core::mem::offset_of!(ProcessorControlRegion, preempt_count) == 32);
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
///
/// Layout pinned by the offset razors in
/// [`offsets::KERNEL_RETURN_CTX`] and the per-field razors below.
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
    /// `&BspToken<'brand>` discharges both contracts: `init_gdt` says
    /// "must be called before install" (this fn pairs them in order)
    /// and `install` says "init_gdt must be called first" (likewise
    /// satisfied). The BSP-init scope guarantees this is the BSP and
    /// the PCR was just minted, so the `&mut self` borrow is the
    /// unique-owner reborrow that boot performs once per CPU.
    pub fn bsp_init_gdt_and_install<'brand>(&mut self, _token: &crate::sync::BspToken<'brand>) {
        // SAFETY: `init_gdt` then `install` pairs the two halves; the
        // `&BspToken` witnesses BSP-init scope (pre-SMP, BSP only).
        unsafe {
            self.init_gdt();
            self.install();
        }
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
    /// Offset of the `preempt_count` field (`AtomicU32`). Consumed as a
    /// `const` operand by the single-instruction per-CPU ops below
    /// (`preempt_count_inc` and friends) so the count is always
    /// manipulated on the CPU executing the instruction.
    pub const PREEMPT_COUNT: usize = 32;
    /// Offset of the `reschedule_pending` field (`AtomicU32`). Consumed
    /// as a `const` operand by the single-instruction per-CPU ops.
    pub const RESCHEDULE_PENDING: usize = 60;
    /// Offset of the `current_task` field (`AtomicPtr<()>`).
    /// Consumed by `__safestack_pointer_address` as a `const` operand
    /// to locate the running task's `unsafe_stack_sp` via
    /// `gs:[CURRENT_TASK]`.
    pub const CURRENT_TASK: usize = 40;
    /// Offset of the `idle_task` field (`AtomicPtr<()>`).  Read by
    /// the scheduler's idle-resolve paths.
    pub const IDLE_TASK: usize = 48;
    /// Offset of the `syscall_pid` field (`AtomicU32`). Consumed as a
    /// `const` operand by the single-instruction accessors so user
    /// pointer validation always reads the executing CPU's value.
    pub const SYSCALL_PID: usize = 88;
    /// Offset of the `user_ctx_ptr` field (`AtomicPtr<UserContext>`).
    /// Read by `__ostd_user_return` to write user state back into the
    /// active `UserContext`.
    pub const USER_CTX_PTR: usize = 96;
    /// Offset of the `kernel_return_ctx` field
    /// (`SyncUnsafeCell<KernelReturnContext>`).  Written by
    /// `execute_round_trip` and consumed by `__ostd_user_return`.
    pub const KERNEL_RETURN_CTX: usize = 104;
    /// Offset of the `user_rax_tmp` PCR scratch slot used by
    /// `__ostd_user_return` to spill user RAX without touching the
    /// kernel stack at `kernel_rsp - 8`.
    pub const USER_RAX_TMP: usize = 168;
    /// Offset of the `ist_unsafe_sp` field — the per-CPU SafeStack
    /// data-stack pointer used while running on an IST/exception stack.
    /// Consumed by `__safestack_pointer_address` as a `const` operand.
    /// Computed (not a literal) because the field is appended after the
    /// 64 KiB embedded kernel stack to keep the asm-critical offsets
    /// (`<= 184`) byte-identical.
    pub const IST_UNSAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, ist_unsafe_sp);
    /// Offset of the `panic_safe_sp` field — the emergency SAFE-stack top the
    /// fatal-fault trampoline loads into `RSP`. Computed, consumed as a `const`
    /// operand by the emergency trampoline asm.
    pub const PANIC_SAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, panic_safe_sp);
    /// Offset of the `panic_unsafe_sp` field — the emergency DATA-stack top the
    /// trampoline writes into the running data-SP slot. Computed.
    pub const PANIC_UNSAFE_SP: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, panic_unsafe_sp);
    /// Offset of the `current_task_id` field (`AtomicU32`). Consumed as a
    /// `const` operand by the single-instruction accessors so the id always
    /// comes from the CPU executing the instruction. Computed, not pinned: no
    /// asm outside this module reads it, so it may move as the tail grows.
    pub const CURRENT_TASK_ID: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, current_task_id);
    /// Offset of the `current_task_priority` field (`AtomicU8`). Same rules as
    /// [`CURRENT_TASK_ID`]: computed, not pinned, and read only through the
    /// single-instruction accessor in this module.
    pub const CURRENT_TASK_PRIORITY: usize =
        core::mem::offset_of!(super::ProcessorControlRegion, current_task_priority);
}

/// IST/exception safe-stack region bounds used by
/// [`super::super::super::arch::x86_64::naked::__safestack_pointer_address`]
/// to decide, purely from the running `RSP`, whether instrumented code is
/// executing on an IST/exception stack (select the per-CPU `ist_unsafe_sp`
/// data stack) or in task/kernel/boot context (select the per-task data
/// stack).
///
/// The canonical layout lives in `slopos_mm::memory_layout_defs`
/// (`EXCEPTION_STACK_REGION_BASE` / `EXCEPTION_STACK_REGION_END`); these are
/// duplicated here because the SafeStack resolver is in OSTD — below `mm`
/// in the crate graph — and must supply them as naked-asm `const` operands.
/// A compile-time razor in `memory_layout_defs.rs` asserts they match, so
/// the two can never drift.
pub const SAFESTACK_IST_REGION_BASE: u64 = 0xFFFF_FFFF_C000_0000;

/// Span of [`SAFESTACK_IST_REGION_BASE`] (256 MiB: `C000_0000..D000_0000`).
pub const SAFESTACK_IST_REGION_SPAN: u64 = 0x1000_0000;

// ==================== STATIC STORAGE ====================

/// BSP's PCR (statically allocated).
///
/// Exported with a stable symbol name so the `_start` assembly
/// trampoline in `boot/limine_entry.s` can reference it via
/// `[rip + BSP_PCR]` to initialise `self_ref`, `unsafe_sp`, and
/// `GS_BASE` *before* the first instrumented Rust function runs.
/// (Every function compiled with `-Zsanitizer=safestack` calls
/// `__safestack_pointer_address()` in its prologue — which reads
/// `gs:[0]` expecting a valid PCR pointer.)
#[unsafe(no_mangle)]
pub static BSP_PCR: SyncUnsafeCell<ProcessorControlRegion> =
    SyncUnsafeCell::new(ProcessorControlRegion::new());

/// Statically-allocated AP PCRs.
///
/// Exported with a stable symbol so the AP naked bootstrap trampoline
/// in `boot/src/smp.rs` can reference individual entries as
/// `[rip + AP_PCRS] + slot * sizeof(PCR)` — though in practice the
/// trampoline goes through the [`AP_PCR_PTRS`] lookup table below
/// because the PCR is ~72 KiB and not a power of two.
#[unsafe(no_mangle)]
pub static AP_PCRS: SyncUnsafeCell<[ProcessorControlRegion; MAX_STATIC_APS]> =
    SyncUnsafeCell::new({
        const INIT: ProcessorControlRegion = ProcessorControlRegion::new();
        [INIT; MAX_STATIC_APS]
    });

/// Lookup table mapping AP slot index (0..MAX_STATIC_APS) to the
/// corresponding AP_PCRS entry pointer.  Populated once on the BSP in
/// [`init_ap_pcr_lookup`] before any AP is started.  The AP bootstrap
/// trampoline (which has to install GS_BASE *before* any instrumented
/// Rust can run — see `crate::task::bootstrap`) uses this table rather
/// than reimplementing "multiply by sizeof(PCR)" in hand-rolled asm.
///
/// Raw pointers are not `Sync`; wrapped in [`PcrPtrLookup`] for a
/// single-writer-during-boot discipline.
#[repr(transparent)]
pub struct PcrPtrLookup(pub [*mut ProcessorControlRegion; MAX_STATIC_APS]);
unsafe impl Sync for PcrPtrLookup {}

#[unsafe(no_mangle)]
pub static AP_PCR_PTRS: SyncUnsafeCell<PcrPtrLookup> =
    SyncUnsafeCell::new(PcrPtrLookup([ptr::null_mut(); MAX_STATIC_APS]));

/// Pre-populate [`AP_PCR_PTRS`] and prime each AP PCR's `self_ref`
/// + `current_task` fields so the naked AP trampoline can install
/// GS_BASE and have `__safestack_pointer_address` find a valid
/// bootstrap task on the very first instrumented call of `ap_entry`.
///
/// Must run on the BSP *before* any AP is started.  Indexed by
/// 0-based AP slot (AP slot i ↔ PCR at `AP_PCRS[i]`).
/// `bootstrap_tasks[i]` is a pointer to the AP's bootstrap Task stub
/// whose `unsafe_stack_sp` has already been primed — see
/// `crate::task::bootstrap::init_bootstrap_tasks`.
///
/// The `&BspToken<'brand>` witnesses BSP-only init; single-writer
/// (BSP in a sequential pre-SMP phase), must be called exactly once.
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
            (*pcr)
                .current_task
                .store(*task, core::sync::atomic::Ordering::Release);
            (*ptrs)[i] = pcr;
        }
    }
}

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
    // NOTE: `self_ref`, `unsafe_sp`, and GS_BASE were already primed by
    // the `_start` asm trampoline in `boot/limine_entry.s` before any
    // instrumented Rust ran.  Re-writing them here is idempotent.
    unsafe {
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

        // Register APIC mapping.
        register_apic_mapping(cpu_id, apic_id);

        pcr
    }
}

/// Safe wrapper around an Application-Processor PCR pointer returned by
/// [`init_ap_pcr`]. Encapsulates the `init_gdt` + `install` sequence so
/// AP-bringup callers don't need to dereference the raw pointer.
pub struct ApPcrHandle {
    ptr: *mut ProcessorControlRegion,
}

unsafe impl Send for ApPcrHandle {}

impl ApPcrHandle {
    /// Allocate and prime the AP's PCR slot. The returned handle must
    /// then have [`Self::init_gdt_and_install`] called on it before any
    /// instrumented Rust runs that observes `gs:[…]`.
    ///
    /// The `&ApToken<'brand>` witnesses AP-init; the underlying
    /// [`init_ap_pcr`] is per-AP one-shot via the AP-init InitFlag.
    pub fn init<'brand>(token: &crate::sync::ApToken<'brand>, apic_id: u32) -> Self {
        let ptr = init_ap_pcr(token, apic_id);
        Self { ptr }
    }

    /// Load this AP's GDT, populate its TSS descriptor, then install
    /// GS_BASE / KERNEL_GS_BASE.
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

// ==================== SINGLE-INSTRUCTION PER-CPU OPS ====================
//
// Migration-atomic accessors for per-CPU PCR scalars (the Linux
// `this_cpu_*` discipline, e.g. `incl %gs:__preempt_count`).
//
// [`current_pcr`] materialises the PCR *pointer* and lets the caller
// dereference it later. In preemptible context (preempt_count == 0,
// IRQs on — the head of every `SpinLock::lock`) an IRQ-driven
// reschedule can migrate the task BETWEEN the pointer fetch and the
// access, landing the access on the previous CPU's PCR. For the
// preempt count that split is fatal: the increment lands on the old
// CPU — which then never preempts again (stranded READY tasks) —
// while the new CPU's count stays 0, so the matching guard drop
// underflows. Each accessor below compiles to a SINGLE gs-relative
// instruction, which the CPU executes atomically with respect to
// interrupts: there is no window in which "this CPU" can change
// between resolving the address and performing the access.
//
// No `lock` prefix on `add`/`xadd` (and none needed): these fields
// are written only by their owning CPU — the preempt guards and
// `switch_context` run on the CPU they account, and
// `reschedule_pending` is set either locally or by the
// reschedule-IPI handler executing on the target CPU. Cross-CPU
// readers (diagnostics) tolerate stale snapshots. x86-TSO plus the
// `asm!` blocks' implicit compiler barrier provide all the ordering
// the previous `Ordering::Acquire`/`Release` atomics provided for
// this CPU-local data.
//
// GS_BASE must point at a valid PCR when any of these run — the same
// precondition `current_pcr` documents; both the BSP and AP entry
// trampolines install GS_BASE before any instrumented Rust executes.

/// Increment this CPU's preempt count by one (single instruction).
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

/// Decrement this CPU's preempt count by one and return the
/// pre-decrement value (single `xadd` instruction).
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

/// Read this CPU's preempt count (single load).
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

/// Overwrite this CPU's preempt count (single store). Only the
/// context-switch count swap may use this, with IRQs disabled.
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

/// Set this CPU's deferred-reschedule flag (single store).
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

/// Clear this CPU's deferred-reschedule flag (single store).
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

/// Read this CPU's deferred-reschedule flag (single load).
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

/// Atomically read-and-clear this CPU's deferred-reschedule flag
/// (single `xchg` instruction — implicitly locked, so a same-instant
/// set from a local IRQ is either observed or preserved, never lost).
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
        if ptr.is_null() { None } else { Some(&*ptr) }
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
    if ptr.is_null() { None } else { Some(&mut *ptr) }
}

/// `get_pcr_mut` exposed as a safe surface for per-CPU init-only
/// mutators (TSS rsp0 update, IST slot binding) that are race-free
/// under Inv. 8: each CPU mutates only its own `cpu_id` slot.
///
/// The name carries `_via_token` historically; the actual gate today
/// is the per-CPU invariant. `cpu_id` validates inside `get_pcr_mut`
/// and out-of-range values return `None`.
pub fn get_pcr_mut_via_token(cpu_id: usize) -> Option<&'static mut ProcessorControlRegion> {
    // SAFETY: per-CPU slot mutation under Inv. 8 — callers commit to
    // writing only `cpu_id`'s slot. The PCR is alive for the kernel
    // lifetime once initialised, so the `'static` reborrow is sound.
    unsafe { get_pcr_mut(cpu_id) }
}

/// Safe surface for "get the *local* CPU's PCR by id". Resolves the
/// current CPU via [`get_current_cpu`] then looks the slot up in the
/// table. Returns `None` if PCR init hasn't run yet.
///
/// Replaces `unsafe { current_pcr() }` for the common pattern of
/// reading the local CPU's PCR fields (counters, flags, atomic
/// scratch slots like `syscall_pid`) — the per-CPU slot is alive for
/// the kernel lifetime and the GS-relative fast path is not actually
/// required for these uses.
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
/// Single-writer-per-CPU by construction: invoked once at boot by
/// `ist_stacks` (before interrupts are enabled, hence before any
/// instrumented exception handler can run on this CPU), and re-primed by
/// `retire_faulted_cpu` — the one exception path that `schedule()`s away
/// without unwinding the IST data stack — so the abandoned depth does not
/// accumulate across successive fatal user faults. The only other writer
/// is the LLVM SafeStack prologue, which runs on this same CPU; callers
/// invoke this with interrupts disabled (boot) or from a divergent
/// exception path that never resumes the abandoned frames.
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

/// Read the current CPU's IST/exception data-stack pointer. Diagnostics
/// only (e.g. overflow reporting); the hot path reads it via the naked
/// `__safestack_pointer_address` asm, never this fn.
#[inline]
pub fn local_ist_unsafe_sp() -> u64 {
    current_pcr_local()
        // SAFETY: per-CPU read of this CPU's slot; benign for diagnostics.
        .map(|pcr| unsafe { *pcr.ist_unsafe_sp.get() })
        .unwrap_or(0)
}

/// Prime this CPU's emergency SAFE-stack top (`PCR.panic_safe_sp`). Called by
/// `ist_stacks` during per-CPU bringup, before any fatal fault can occur.
#[inline]
pub fn set_local_panic_safe_sp(top: u64) {
    if let Some(pcr) = current_pcr_local() {
        // SAFETY: per-CPU slot write of the owning CPU's PCR (Inv. 8).
        unsafe {
            *pcr.panic_safe_sp.get() = top;
        }
    }
}

/// Prime this CPU's emergency DATA-stack top (`PCR.panic_unsafe_sp`).
#[inline]
pub fn set_local_panic_unsafe_sp(top: u64) {
    if let Some(pcr) = current_pcr_local() {
        // SAFETY: per-CPU slot write of the owning CPU's PCR (Inv. 8).
        unsafe {
            *pcr.panic_unsafe_sp.get() = top;
        }
    }
}

/// Read this CPU's emergency SAFE-stack top (diagnostics / tests).
#[inline]
pub fn local_panic_safe_sp() -> u64 {
    current_pcr_local()
        .map(|pcr| unsafe { *pcr.panic_safe_sp.get() })
        .unwrap_or(0)
}

/// Read this CPU's emergency DATA-stack top (diagnostics / tests).
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

/// Enter the fatal-panic path on this CPU, returning the PREVIOUS depth. A
/// non-zero return means the fatal path itself faulted (recursion) and the
/// caller must degrade to the format-free abort. Never decremented — a CPU
/// that enters the fatal path does not leave it.
#[inline]
pub fn panic_depth_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_depth.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Enter the panic handler on this CPU, returning the previous in-flight
/// depth. A non-zero previous value means the panic handler re-entered before
/// the prior panic reached an unwind catch boundary or a fatal halt.
#[inline]
pub fn panic_in_flight_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_in_flight.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Leave the panic handler after a caught panic crosses an unwind catch
/// boundary. Returns the previous in-flight depth and saturates at zero.
#[inline]
pub fn panic_in_flight_exit() -> u32 {
    current_pcr_local()
        .map(|pcr| counter_exit_saturating(&pcr.panic_in_flight))
        .unwrap_or(0)
}

/// Current panic-handler in-flight depth for this CPU.
#[inline]
pub fn panic_in_flight_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.panic_in_flight.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Install a task's saved panic in-flight depth on this CPU. Context-switch
/// use only: an unwinding task runs interrupts-on and can migrate, so the
/// depth must travel with the task or the enter-CPU's counter leaks and a
/// later `AbortOnUnwind` drop on that CPU false-aborts.
#[inline]
pub fn panic_in_flight_store(depth: u32) {
    if let Some(pcr) = current_pcr_local() {
        pcr.panic_in_flight.store(depth, Ordering::Release);
    }
}

/// Enter interrupt/exception context on this CPU, returning the previous
/// nesting depth.
#[inline]
pub fn interrupt_nesting_enter() -> u32 {
    current_pcr_local()
        .map(|pcr| {
            pcr.in_interrupt.store(true, Ordering::SeqCst);
            pcr.interrupt_nesting.fetch_add(1, Ordering::SeqCst)
        })
        .unwrap_or(0)
}

/// Leave interrupt/exception context on this CPU. Returns the previous nesting
/// depth and saturates at zero.
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

/// Current interrupt/exception nesting depth for this CPU.
#[inline]
pub fn interrupt_nesting_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.interrupt_nesting.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// True if this CPU is currently in interrupt/exception context.
#[inline]
pub fn in_interrupt_context() -> bool {
    current_pcr_local()
        .map(|pcr| {
            pcr.interrupt_nesting.load(Ordering::Acquire) != 0
                || pcr.in_interrupt.load(Ordering::Acquire)
        })
        .unwrap_or(false)
}

/// Enter a panic-recovery scope on this CPU, returning the previous recovery
/// depth.
#[inline]
pub fn recovery_depth_enter() -> u32 {
    recovery_depth_enter_for_cpu(get_current_cpu())
}

/// Leave a panic-recovery scope on this CPU. Returns the previous recovery
/// depth and saturates at zero.
#[inline]
pub fn recovery_depth_exit() -> u32 {
    recovery_depth_exit_for_cpu(get_current_cpu())
}

/// Current panic-recovery depth for this CPU.
#[inline]
pub fn recovery_depth() -> u32 {
    current_pcr_local()
        .map(|pcr| pcr.recovery_depth.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Panic-recovery depth for `cpu_id` (cross-CPU read; `0` for an unknown or
/// offline CPU). Non-zero means that CPU is inside a `run_recoverable` scope,
/// where a caught panic may run the DWARF unwinder with interrupts disabled
/// (and thus without advancing its per-CPU timer tick) for a legitimately long
/// time. The NMI watchdog reads this to grant that CPU a bounded grace before
/// treating it as stuck.
#[inline]
pub fn recovery_depth_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| pcr.recovery_depth.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Replace this CPU's live panic-recovery depth.
#[inline]
pub fn recovery_depth_store(depth: u32) {
    if let Some(pcr) = current_pcr_local() {
        pcr.recovery_depth.store(depth, Ordering::Release);
    }
}

/// Enter a panic-recovery scope for an explicit CPU id. Used by recovery
/// guards so Drop exits the same per-CPU slot that was entered.
#[doc(hidden)]
#[inline]
pub fn recovery_depth_enter_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| pcr.recovery_depth.fetch_add(1, Ordering::SeqCst))
        .unwrap_or(0)
}

/// Leave a panic-recovery scope for an explicit CPU id. Used by recovery
/// guards so Drop exits the same per-CPU slot that was entered.
#[doc(hidden)]
#[inline]
pub fn recovery_depth_exit_for_cpu(cpu_id: usize) -> u32 {
    get_pcr(cpu_id)
        .map(|pcr| counter_exit_saturating(&pcr.recovery_depth))
        .unwrap_or(0)
}

/// Raw store of `top` into this CPU's `PCR.ist_unsafe_sp`, bypassing the
/// SafeStack sanitizer.
///
/// Used to re-prime the exception data-stack pointer when an exception
/// handler abandons it without unwinding (`retire_faulted_cpu`'s divergent
/// `schedule()`). Why naked: that abandoning code runs *on* an IST stack,
/// so `__safestack_pointer_address` resolves THIS very slot as its own
/// data-SP — an instrumented setter's SafeStack epilogue would restore the
/// slot to the setter's entry value and silently undo the re-prime. A naked
/// fn has no prologue/epilogue, so its store sticks.
///
/// Call it DIRECTLY from the abandoning path. Wrapping it in an
/// instrumented helper re-introduces the same epilogue clobber (the
/// wrapper, also running on the IST stack, would restore the slot on
/// return), so there is intentionally no safe wrapper. Clobbers only `rax`.
///
/// Distinct from [`set_local_ist_unsafe_sp`], which is correct for *boot*
/// priming because that runs on the boot/kernel stack — there
/// `__safestack_pointer_address` resolves the per-task slot, not this one,
/// so the setter's own frame never touches `ist_unsafe_sp`.
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
