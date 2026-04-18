//! Kernel-internal task structures.
//!
//! Contains the `Task` struct, CPU register contexts, and FPU state that are
//! used exclusively by kernel subsystems. The ABI-stable enums and constants
//! remain in `slopos_abi::task`.

use core::ffi::c_void;
use core::mem::offset_of;
use core::ptr;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_abi::signal::{NSIG, SIG_DFL, SIG_EMPTY, SigSet};
use slopos_abi::syscall::TtyIndex;
use slopos_alloc::{AllocError, Init, init_from_closure};

pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_FPU_INITIALIZED, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE,
    TASK_NAME_MAX_LEN, TASK_PRIORITY_HIGH, TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW,
    TASK_PRIORITY_NORMAL, TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord,
    TaskFaultReason, TaskStatus,
};

// =============================================================================
// TaskContext — full CPU register state for interrupt-driven context switches
// =============================================================================

/// CPU register state saved during context switches.
/// Size: 200 bytes (0xC8) — 25 × 8-byte registers.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct TaskContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
    pub ss: u64,
    pub cr3: u64,
}

impl TaskContext {
    pub const fn zero() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            rsp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
            cs: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
            ss: 0,
            cr3: 0,
        }
    }
}

// =============================================================================
// SwitchContext — callee-saved registers for software context switch
// =============================================================================

/// Layout must match the assembly in `context_switch.s` and `switch_asm.rs`.
/// Compile-time assertions below verify every offset.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SwitchContext {
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub rip: u64,
}

impl SwitchContext {
    pub const fn zero() -> Self {
        Self {
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rflags: 0x202,
            rip: 0,
        }
    }

    pub const fn new_for_task(entry_point: u64, arg: u64, stack_top: u64, trampoline: u64) -> Self {
        Self {
            rbx: 0,
            r12: entry_point,
            r13: arg,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: stack_top - 8,
            rflags: 0x202,
            rip: trampoline,
        }
    }
}

const _: () = assert!(core::mem::size_of::<SwitchContext>() == 72);

pub const SWITCH_CTX_OFF_RBX: usize = 0;
pub const SWITCH_CTX_OFF_R12: usize = 8;
pub const SWITCH_CTX_OFF_R13: usize = 16;
pub const SWITCH_CTX_OFF_R14: usize = 24;
pub const SWITCH_CTX_OFF_R15: usize = 32;
pub const SWITCH_CTX_OFF_RBP: usize = 40;
pub const SWITCH_CTX_OFF_RSP: usize = 48;
pub const SWITCH_CTX_OFF_RFLAGS: usize = 56;
pub const SWITCH_CTX_OFF_RIP: usize = 64;

const _: () = {
    assert!(offset_of!(SwitchContext, rbx) == SWITCH_CTX_OFF_RBX);
    assert!(offset_of!(SwitchContext, r12) == SWITCH_CTX_OFF_R12);
    assert!(offset_of!(SwitchContext, r13) == SWITCH_CTX_OFF_R13);
    assert!(offset_of!(SwitchContext, r14) == SWITCH_CTX_OFF_R14);
    assert!(offset_of!(SwitchContext, r15) == SWITCH_CTX_OFF_R15);
    assert!(offset_of!(SwitchContext, rbp) == SWITCH_CTX_OFF_RBP);
    assert!(offset_of!(SwitchContext, rsp) == SWITCH_CTX_OFF_RSP);
    assert!(offset_of!(SwitchContext, rflags) == SWITCH_CTX_OFF_RFLAGS);
    assert!(offset_of!(SwitchContext, rip) == SWITCH_CTX_OFF_RIP);
};

// =============================================================================
// FpuState — XSAVE/FXSAVE area for x87/SSE/AVX state (64-byte aligned)
// =============================================================================

/// Maximum XSAVE area size we support (compile-time upper bound).
///
/// Covers all current Intel/AMD XSAVE components:
/// - FXSAVE (x87 + SSE):    512 bytes
/// - XSAVE  (+ AVX):        832 bytes
/// - XSAVE  (+ AVX-512):  2,688 bytes
///
/// Tasks always allocate this much; the hardware only touches
/// `xsave::area_size()` bytes at runtime.  The waste-per-task is at
/// most ~2 KiB — acceptable for a fixed task table.
pub const FPU_STATE_SIZE: usize = 2688;

/// Legacy FXSAVE area size (512 B).  Used as the fallback when XSAVE
/// is not available.
pub const FXSAVE_AREA_SIZE: usize = 512;

pub const MXCSR_DEFAULT: u32 = 0x1F80;

/// x87 FPU Control Word offset within both FXSAVE and XSAVE legacy region.
const LEGACY_FCW_OFFSET: usize = 0;
/// MXCSR offset within both FXSAVE and XSAVE legacy region.
const LEGACY_MXCSR_OFFSET: usize = 24;

// --- XSAVE header layout (bytes 512–575) ---

/// Offset of the XSAVE header within the save area.
const XSAVE_HEADER_OFFSET: usize = 512;
/// XSTATE_BV — bitmask of state components that contain valid data.
const XSTATE_BV_OFFSET: usize = XSAVE_HEADER_OFFSET;
/// XCOMP_BV — bitmask for compacted format (XSAVEC).  Bit 63 = compaction mode.
const XCOMP_BV_OFFSET: usize = XSAVE_HEADER_OFFSET + 8;

/// FPU/SIMD state save area.
///
/// Sized to `FPU_STATE_SIZE` (compile-time maximum for XSAVE with AVX-512)
/// and aligned to 64 bytes as required by the `XSAVE`/`XRSTOR` instructions.
/// When the CPU only supports FXSAVE, only the first 512 bytes are used.
///
/// The layout is intentionally a plain byte array so it can back both the
/// 512-byte FXSAVE region and the variable-length XSAVE region without
/// requiring separate types.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct FpuState {
    pub data: [u8; FPU_STATE_SIZE],
}

impl FpuState {
    /// All-zeroes save area.
    pub const fn zero() -> Self {
        Self {
            data: [0u8; FPU_STATE_SIZE],
        }
    }

    /// Default FPU state with x87/SSE exceptions masked and XSAVE header zeroed.
    ///
    /// Initialises the legacy region (bytes 0–511):
    ///   - FCW = 0x037F (all x87 exceptions masked)
    ///   - MXCSR = 0x1F80 (all SSE exceptions masked)
    ///
    /// The XSAVE header (bytes 512–575) is left zeroed:
    ///   - `XSTATE_BV = 0` → all state components at initial values
    ///   - `XCOMP_BV = 0` → standard (non-compacted) format
    ///
    /// This is correct for both FXSAVE and XSAVE: the hardware interprets
    /// `XSTATE_BV = 0` as "use processor-reset defaults for every component"
    /// during `XRSTOR`.
    pub const fn new() -> Self {
        let mut state = Self::zero();
        // Legacy region: FCW and MXCSR defaults (same for FXSAVE and XSAVE).
        state.data[LEGACY_FCW_OFFSET] = 0x7F;
        state.data[LEGACY_FCW_OFFSET + 1] = 0x03;
        state.data[LEGACY_MXCSR_OFFSET] = 0x80;
        state.data[LEGACY_MXCSR_OFFSET + 1] = 0x1F;
        // XSAVE header: XSTATE_BV = 0, XCOMP_BV = 0 (already zero from zero()).
        // Explicit documentation — no-ops but make the invariant visible.
        state.data[XSTATE_BV_OFFSET] = 0;
        state.data[XCOMP_BV_OFFSET] = 0;
        state
    }

    /// Initialise an `FpuState` directly at `ptr` without materialising the
    /// 2.6 KiB rvalue on the caller's stack. Equivalent to writing the
    /// result of [`Self::new`] but with no temp.
    ///
    /// The assembly in `context_switch.s` relies on `FpuState` living
    /// inline at `FPU_STATE_OFFSET` from the `TaskContext` field, so the
    /// in-place factory is the right shape — `KBox<FpuState>` would force
    /// asm changes for no extra stack-safety win once the rvalue is gone.
    ///
    /// # Safety
    /// `ptr` must be a valid, properly-aligned, writable pointer to an
    /// `FpuState`-sized region (≥ `FPU_STATE_SIZE` bytes, 64-byte
    /// aligned). The caller must ensure no other reference to that region
    /// is live for the duration of this call.
    pub unsafe fn reset_in_place(ptr: *mut Self) {
        // SAFETY: ptr is a valid FpuState by caller contract.
        unsafe {
            let bytes = ptr as *mut u8;
            core::ptr::write_bytes(bytes, 0u8, FPU_STATE_SIZE);
            // Legacy FCW = 0x037F, MXCSR = 0x1F80.
            *bytes.add(LEGACY_FCW_OFFSET) = 0x7F;
            *bytes.add(LEGACY_FCW_OFFSET + 1) = 0x03;
            *bytes.add(LEGACY_MXCSR_OFFSET) = 0x80;
            *bytes.add(LEGACY_MXCSR_OFFSET + 1) = 0x1F;
        }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Returns the number of bytes the hardware will actually touch during
    /// `XSAVE`/`XRSTOR` (or `FXSAVE`/`FXRSTOR` on fallback).
    ///
    /// Always ≤ `FPU_STATE_SIZE`.
    #[inline]
    pub fn active_area_size() -> usize {
        slopos_arch::cpu::xsave::area_size()
    }
}

impl Default for FpuState {
    fn default() -> Self {
        Self::new()
    }
}

// Compile-time checks on FpuState.
const _: () = {
    // XSAVE requires 64-byte alignment.
    assert!(core::mem::align_of::<FpuState>() >= 64);
    // Buffer must be large enough for the FXSAVE legacy area.
    assert!(FPU_STATE_SIZE >= FXSAVE_AREA_SIZE);
    // Buffer size should be a multiple of the alignment for clean packing.
    assert!(FPU_STATE_SIZE % core::mem::align_of::<FpuState>() == 0);
};

/// Panics at boot if the CPU's XSAVE area exceeds our compile-time maximum.
///
/// Call once from a boot step (after `xsave::init()`) to fail early rather
/// than silently corrupting adjacent task memory.
pub fn validate_fpu_state_size() {
    let hw_size = slopos_arch::cpu::xsave::area_size();
    assert!(
        hw_size <= FPU_STATE_SIZE,
        "XSAVE area size ({} B) exceeds compile-time FPU_STATE_SIZE ({} B) — \
         increase FPU_STATE_SIZE in task_struct.rs",
        hw_size,
        FPU_STATE_SIZE,
    );
}

// =============================================================================
// SignalAction — kernel-internal per-signal disposition
// =============================================================================

/// Kernel-internal signal action. Mirrors the relevant fields of UserSigaction
/// but stored per-task for fast dispatch.
#[derive(Copy, Clone)]
pub struct SignalAction {
    /// Handler address: SIG_DFL (0), SIG_IGN (1), or a user function pointer.
    pub handler: u64,
    /// Signal mask to OR into blocked set while handler runs.
    pub mask: SigSet,
    /// SA_* flags (SA_RESTORER, SA_NODEFER, SA_RESETHAND, etc.)
    pub flags: u64,
    /// Restorer function pointer (set via SA_RESTORER).
    pub restorer: u64,
}

impl SignalAction {
    pub const fn default() -> Self {
        Self {
            handler: SIG_DFL,
            mask: SIG_EMPTY,
            flags: 0,
            restorer: 0,
        }
    }
}

// =============================================================================
// Task — the kernel task control block
// =============================================================================

// Verify assembly FPU_STATE_OFFSET matches the actual field distance.
// Assembly in context_switch.s uses `.equ FPU_STATE_OFFSET, <value>`.
// Changing FpuState alignment from 16 to 64 may alter the padding and thus this offset.
// We use a helper const to make the actual value available for debugging.
pub const FPU_STATE_OFFSET: usize = {
    // This value MUST match `offset_of!(Task, fpu_state) - offset_of!(Task, context)`.
    // The assembly in context_switch.s uses `.equ FPU_STATE_OFFSET, <hex>`.
    0xD0
};
const _: () = assert!(offset_of!(Task, fpu_state) - offset_of!(Task, context) == FPU_STATE_OFFSET);

/// Offset of `Task.unsafe_stack_sp` — consumed by the naked
/// `__safestack_pointer_address` trampoline in `karch::safestack_rt`
/// as a `const` operand, so the asm cannot drift out of sync with the
/// struct layout.
///
/// The slot is **task-local** (not per-CPU) on purpose: LLVM's
/// `-safestack-use-pointer-address` mode caches the slot pointer on
/// the safe stack across calls, and a per-CPU slot address would
/// become stale the moment a task migrates between CPUs.  Embedding
/// the slot inside the Task struct means the address survives every
/// migration — the Task heap allocation never moves.
pub const TASK_UNSAFE_STACK_SP_OFFSET: usize = offset_of!(Task, unsafe_stack_sp);
// Tripwire: the Task struct is inherently large (dominated by FpuState).
// The stack-safety contract is that nobody ever materialises a Task
// rvalue on the stack — see `Task::reset_in_place`. This bound keeps
// Task from growing past one full memory page so its static array
// fits comfortably in `.bss` and heap callers budget a single-page
// allocation when they need a scratch slot. Adjust only in concert
// with a re-audit of every Task mutation site.
const _: () = assert!(core::mem::size_of::<Task>() <= 8192);

#[repr(C)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    state_atomic: AtomicU8,
    pub priority: u8,
    pub flags: u16,
    block_reason: AtomicU8,
    _pad0: [u8; 3],
    pub process_id: u32,
    pub stack_base: u64,
    pub stack_size: u64,
    pub stack_pointer: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,
    pub entry_point: u64,
    pub entry_arg: *mut c_void,
    pub context: TaskContext,
    pub fpu_state: FpuState,
    // --- Fields below are NOT accessed by assembly and can be freely reordered ---
    /// Owning handle to the kernel-mode stack.
    ///
    /// `Some` for every live task; `None` only on `invalid()` slots and
    /// freed tasks (after `free_task_stacks`).  Dropping the `KernelStack`
    /// unmaps the stack pages, returns the physical frames to the page
    /// allocator, and releases the VA slot — so freeing a task is just
    /// `task.kernel_stack = None`.
    ///
    /// The adjacent raw `kernel_stack_base / _top / _size` fields above
    /// are populated from this handle at `task_create` time and must
    /// remain consistent with it while the task is live.  They exist
    /// because some assembly / syscall paths read them as plain `u64`
    /// rather than going through the handle.
    pub kernel_stack: Option<crate::scheduler::stack::KernelStack>,
    /// Owning handle to the SafeStack-sanitizer unsafe (data) stack.
    ///
    /// Lives alongside `kernel_stack` — allocated at task creation,
    /// dropped in `reset_in_place`/Drop like `kernel_stack`.  While the
    /// task is running, its top-of-stack pointer is copied into the
    /// current CPU's per-CPU `unsafe_sp` slot (inside the PCR) so that
    /// LLVM-emitted instrumentation can find it via
    /// `__safestack_pointer_address`; see `safestack_rt` in the `karch`
    /// crate.  Context switch saves/restores `unsafe_stack_sp` to/from
    /// the PCR slot exactly the same way the CPU's regular RSP is
    /// saved/restored to/from `switch_ctx.rsp`.
    pub unsafe_stack: Option<crate::scheduler::unsafe_stack::UnsafeStack>,
    /// Current SafeStack unsafe-stack pointer (data-stack RSP).
    ///
    /// Initialised to `unsafe_stack.as_ref().unwrap().top()` at task
    /// creation and then advanced/retreated by the SafeStack sanitizer
    /// on every instrumented function prologue/epilogue.  Saved on
    /// switch-out, restored on switch-in.
    pub unsafe_stack_sp: u64,
    /// Index of this Task's slot in the `TASK_MANAGER` pool spine,
    /// populated by `reserve_task_slot` and invariant for the lifetime
    /// of the Task. `u32::MAX` on fresh/invalid slots that have not
    /// been assigned a pool index yet. Gives O(1) lookup of the owning
    /// pool slot from a Task pointer, used for `exit_records`
    /// parallel-indexing and pointer-validity checks.
    pub slot_index: u32,
    pub parent_task_id: u32,
    /// FS segment base address (TLS pointer). Written to MSR FS_BASE before
    /// switching to user mode, and read back on context save.
    pub fs_base: u64,
    /// Thread-group ID. For the group leader, tgid == task_id.
    /// For threads created with CLONE_THREAD, tgid == leader's task_id.
    pub tgid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub controlling_tty: Option<TtyIndex>,
    /// Current working directory path (null-terminated, max 256 bytes).
    /// Initialized to "/" on task creation. Inherited from parent on fork/spawn.
    pub cwd: [u8; 256],
    pub cwd_len: u16,
    /// User-space address to clear (and futex-wake) on thread exit.
    /// Set by clone(CLONE_CHILD_CLEARTID). 0 means not set.
    pub clear_child_tid: u64,
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub waiting_on: AtomicU32,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
    pub cpu_affinity: u32,
    pub last_cpu: u8,
    pub migration_count: u32,
    // --- Signal state ---
    /// Bitmask of pending signals (written atomically by kill()).
    pub signal_pending: AtomicU64,
    /// Bitmask of blocked signals (modified by rt_sigprocmask).
    pub signal_blocked: SigSet,
    /// Per-signal action table.
    pub signal_actions: [SignalAction; NSIG],
    pub switch_ctx: SwitchContext,
    /// Set while a CPU is physically executing this task (context switch
    /// in progress or task running).  Cleared after the outgoing context
    /// switch completes.  `schedule_task` spin-waits on this flag so a
    /// woken task is never dispatched on a second CPU before the first
    /// CPU finishes saving its context — the Linux `p->on_cpu` pattern.
    pub on_cpu: AtomicBool,
    pub next_ready: *mut Task,
    pub next_inbox: AtomicPtr<Task>,
    pub refcnt: AtomicU32,
}

impl Task {
    pub const fn invalid() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            name: [0; TASK_NAME_MAX_LEN],
            state_atomic: AtomicU8::new(TaskStatus::Invalid.as_u8()),
            priority: TASK_PRIORITY_NORMAL,
            flags: 0,
            block_reason: AtomicU8::new(BlockReason::None.as_u8()),
            _pad0: [0; 3],
            process_id: INVALID_PROCESS_ID,
            stack_base: 0,
            stack_size: 0,
            stack_pointer: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            kernel_stack_size: 0,
            entry_point: 0,
            entry_arg: ptr::null_mut(),
            context: TaskContext::zero(),
            fpu_state: FpuState::new(),
            kernel_stack: None,
            unsafe_stack: None,
            unsafe_stack_sp: 0,
            slot_index: u32::MAX,
            parent_task_id: INVALID_TASK_ID,
            fs_base: 0,
            tgid: INVALID_TASK_ID,
            pgid: INVALID_TASK_ID,
            sid: INVALID_TASK_ID,
            controlling_tty: None,
            cwd: {
                let mut c = [0u8; 256];
                c[0] = b'/';
                c
            },
            cwd_len: 1,
            clear_child_tid: 0,
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: 0,
            creation_time: 0,
            yield_count: 0,
            last_run_timestamp: 0,
            waiting_on: AtomicU32::new(INVALID_TASK_ID),
            user_started: 0,
            context_from_user: 0,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
            fate_token: 0,
            fate_value: 0,
            fate_pending: 0,
            cpu_affinity: 0,
            last_cpu: 0,
            migration_count: 0,
            signal_pending: AtomicU64::new(0),
            signal_blocked: SIG_EMPTY,
            signal_actions: [SignalAction::default(); NSIG],
            switch_ctx: SwitchContext::zero(),
            on_cpu: AtomicBool::new(false),
            next_ready: ptr::null_mut(),
            next_inbox: AtomicPtr::new(ptr::null_mut()),
            refcnt: AtomicU32::new(0),
        }
    }

    /// In-place Init recipe for a fresh `Invalid` Task, equivalent in
    /// observable state to [`Task::invalid`] but constructed field-by-field
    /// at the destination slot — no 3.8 KiB rvalue on the caller's stack.
    ///
    /// Used by `KBox::try_init(Task::init_invalid())` when the task pool
    /// grows a fresh slot. The closure writes every field of `slot`
    /// through `addr_of_mut!` so the stack-size gate stays under 2 KiB
    /// even on the unoptimised debug build.
    pub fn init_invalid() -> impl Init<Self, AllocError> {
        // SAFETY: the closure writes every byte of `slot` — first via
        // `write_bytes` to zero the struct, then targeted writes for the
        // fields whose valid `Invalid` value is not all-zero. Returns
        // `Ok(())` only after every write has completed.
        unsafe {
            init_from_closure(|slot: *mut Self| -> Result<(), AllocError> {
                // Zero the entire struct, giving every Atomic, padding
                // byte, and pointer a defined all-zero state.
                core::ptr::write_bytes(slot as *mut u8, 0, core::mem::size_of::<Self>());

                // Scalar fields whose `Invalid` value is non-zero.
                addr_of_mut!((*slot).task_id).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).priority).write(TASK_PRIORITY_NORMAL);
                addr_of_mut!((*slot).process_id).write(INVALID_PROCESS_ID);
                addr_of_mut!((*slot).entry_arg).write(ptr::null_mut());
                addr_of_mut!((*slot).parent_task_id).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).tgid).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).pgid).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).sid).write(INVALID_TASK_ID);

                // Option<TtyIndex>: non-niche Option layout — discriminant
                // byte 0 = None, which matches the zero-fill above, but
                // write explicitly for clarity and layout-independence.
                addr_of_mut!((*slot).controlling_tty).write(None);

                // cwd = "/", cwd_len = 1.
                addr_of_mut!((*slot).cwd_len).write(1);
                // Write the first byte of cwd directly; the remaining
                // 255 bytes stay zero from the initial write_bytes.
                (addr_of_mut!((*slot).cwd) as *mut u8).write(b'/');

                // Waiting-on sentinel.
                addr_of_mut!((*slot).waiting_on).write(AtomicU32::new(INVALID_TASK_ID));

                // FPU state: FCW = 0x037F, MXCSR = 0x1F80, XSAVE header
                // zeroed — handled by FpuState's own in-place initialiser.
                FpuState::reset_in_place(addr_of_mut!((*slot).fpu_state));

                // Kernel stack: no handle yet.
                addr_of_mut!((*slot).kernel_stack).write(None);
                // Unsafe stack (SafeStack data stack): no handle yet.
                addr_of_mut!((*slot).unsafe_stack).write(None);
                addr_of_mut!((*slot).unsafe_stack_sp).write(0);

                // Pool-slot index sentinel: set by `reserve_task_slot`
                // once the slot is assigned.
                addr_of_mut!((*slot).slot_index).write(u32::MAX);

                // Signal bookkeeping: SigSet and SignalAction are all-zero
                // for the invalid disposition (SIG_EMPTY = 0, SIG_DFL = 0,
                // default flags/mask/restorer = 0). The initial write_bytes
                // already satisfies this; an explicit belt-and-braces write
                // follows for layout-independence.
                addr_of_mut!((*slot).signal_blocked).write(SIG_EMPTY);
                for i in 0..NSIG {
                    let p = (addr_of_mut!((*slot).signal_actions) as *mut SignalAction).add(i);
                    p.write(SignalAction::default());
                }

                // Software-switch context: rflags default is 0x202
                // (IF=1, reserved bit1=1) rather than 0.
                addr_of_mut!((*slot).switch_ctx).write(SwitchContext::zero());

                Ok(())
            })
        }
    }

    /// Reset a Task slot in place to the `invalid` state.
    ///
    /// `*slot = Task::invalid()` materialises a ~3.8 KiB Task rvalue on
    /// the caller's stack before the assignment; this primitive skips
    /// that rvalue entirely. Owned resources the Task holds (currently
    /// just `kernel_stack: Option<KernelStack>`) are released
    /// explicitly via field-level `take()` before the rest of the
    /// struct is zero-overwritten, matching the old assignment's drop
    /// semantics without running a full `Task::drop` that might
    /// re-release already-freed state when called on a slot that has
    /// been partially cleaned up through other paths.
    ///
    /// # Safety
    /// - `this` must be non-null, aligned, and point to a writable
    ///   `Task` slot that the caller has exclusive access to.
    /// - The slot must currently hold a valid `Task`.
    pub unsafe fn reset_in_place(this: *mut Task) {
        unsafe {
            // Preserve the pool slot index across the reset — the Task
            // is still owned by the same pool slot after reset, and
            // reserving this identity means callers (e.g. zombie reap)
            // don't need to re-learn the Task's position in the pool.
            let preserved_slot_index = (*this).slot_index;
            // Release the owning Option fields up front. `Option::take`
            // drops the old `Some(..)` exactly once and is a no-op if
            // the field is already `None` (e.g. the caller ran
            // `free_task_stacks` first).  The two stacks are adjacent
            // in the struct so we keep one hole in the byte-zero pass
            // that spans both.
            let _ = (*this).kernel_stack.take();
            let _ = (*this).unsafe_stack.take();
            // Zero the non-drop-bearing fields. We stay away from the
            // `kernel_stack`/`unsafe_stack` slots because `Option` has
            // no layout guarantee that the all-zeros bit pattern is
            // `None` — overwriting the bytes would corrupt the
            // `None` discriminant we just established via `take()`.
            let bytes = core::mem::size_of::<Task>();
            let kernel_stack_off = core::mem::offset_of!(Task, kernel_stack);
            let unsafe_stack_off = core::mem::offset_of!(Task, unsafe_stack);
            let unsafe_stack_size =
                core::mem::size_of::<Option<crate::scheduler::unsafe_stack::UnsafeStack>>();
            debug_assert!(
                kernel_stack_off < unsafe_stack_off,
                "Task: kernel_stack must precede unsafe_stack for reset_in_place hole span"
            );
            let tail_start = unsafe_stack_off + unsafe_stack_size;
            let base = this as *mut u8;
            core::ptr::write_bytes(base, 0, kernel_stack_off);
            core::ptr::write_bytes(base.add(tail_start), 0, bytes - tail_start);
            (*this).task_id = INVALID_TASK_ID;
            (*this).priority = TASK_PRIORITY_NORMAL;
            (*this).process_id = INVALID_PROCESS_ID;
            (*this).slot_index = preserved_slot_index;
            (*this).parent_task_id = INVALID_TASK_ID;
            (*this).tgid = INVALID_TASK_ID;
            (*this).pgid = INVALID_TASK_ID;
            (*this).sid = INVALID_TASK_ID;
            (*this).cwd[0] = b'/';
            (*this).cwd_len = 1;
            (*this).waiting_on.store(INVALID_TASK_ID, Ordering::Relaxed);
        }
    }

    #[inline]
    fn state(&self) -> u8 {
        self.state_atomic.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn set_state(&self, state: u8) {
        self.state_atomic.store(state, Ordering::Release);
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        TaskStatus::from_u8(self.state())
    }

    #[inline]
    pub fn set_status(&self, status: TaskStatus) {
        self.set_state(status.as_u8());
    }

    #[inline]
    pub fn try_transition_to(&self, target: TaskStatus) -> bool {
        let current = self.state();
        let current_status = TaskStatus::from_u8(current);
        if !current_status.can_transition_to(target) {
            return false;
        }
        self.state_atomic
            .compare_exchange(current, target.as_u8(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Atomically transition from `expected` to `target`.
    ///
    /// Unlike [`try_transition_to`], this CAS only succeeds when the current
    /// state is exactly `expected`. This is critical for wakeup-safe blocking:
    /// `try_transition_from(WillBlock, Blocked)` fails if a concurrent
    /// `unblock_task` already set the state to `Running`.
    #[inline]
    pub fn try_transition_from(&self, expected: TaskStatus, target: TaskStatus) -> bool {
        if !expected.can_transition_to(target) {
            return false;
        }
        self.state_atomic
            .compare_exchange(
                expected.as_u8(),
                target.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Block from a specific expected state, setting the block reason.
    ///
    /// Stores `reason` *before* the CAS so the Release ordering on
    /// `state_atomic` publishes it to any CPU that Acquire-loads the
    /// Blocked state.  If the CAS fails the stale reason is harmless
    /// because no reader inspects `block_reason` unless `state == Blocked`.
    ///
    /// Returns `true` only if the CAS `expected → Blocked` succeeded.
    #[inline]
    pub fn block_from(&self, expected: TaskStatus, reason: BlockReason) -> bool {
        self.block_reason.store(reason.as_u8(), Ordering::Relaxed);
        self.try_transition_from(expected, TaskStatus::Blocked)
    }

    #[inline]
    pub fn mark_ready(&self) -> bool {
        self.try_transition_to(TaskStatus::Ready)
    }

    #[inline]
    pub fn mark_running(&self) -> bool {
        self.try_transition_to(TaskStatus::Running)
    }

    #[inline]
    pub fn block(&self, reason: BlockReason) -> bool {
        self.block_reason.store(reason.as_u8(), Ordering::Relaxed);
        self.try_transition_to(TaskStatus::Blocked)
    }

    /// Load the block reason.  Only meaningful when `status() == Blocked`.
    ///
    /// Callers must first Acquire-load `state_atomic` (via `status()`,
    /// `is_blocked()`, etc.) to synchronise with the writer's Release CAS.
    #[inline]
    pub fn load_block_reason(&self) -> BlockReason {
        BlockReason::from_u8(self.block_reason.load(Ordering::Relaxed))
    }

    /// Store the block reason directly (e.g. for futex, which sets the
    /// reason before the generic block path runs).
    #[inline]
    pub fn store_block_reason(&self, reason: BlockReason) {
        self.block_reason.store(reason.as_u8(), Ordering::Relaxed);
    }

    #[inline]
    pub fn terminate(&self) -> bool {
        self.try_transition_to(TaskStatus::Terminated)
    }

    #[inline]
    pub fn is_blocked(&self) -> bool {
        self.status() == TaskStatus::Blocked
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        self.status() == TaskStatus::Ready
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        self.status() == TaskStatus::Running
    }

    #[inline]
    pub fn is_terminated(&self) -> bool {
        self.status() == TaskStatus::Terminated
    }

    /// Bulk-copy task state using `ptr::copy_nonoverlapping`, then reset
    /// linkage, refcount, and owned resources. Replaces the old 44-field
    /// manual `clone_from`.
    ///
    /// # Safety
    /// Caller must ensure `self` and `other` do not overlap and that `self`
    /// is not concurrently accessed by another CPU.
    ///
    /// The byte copy bitwise-duplicates non-trivially-owned fields such as
    /// `kernel_stack: Option<KernelStack>`.  Letting those duplicate values
    /// drop would free the parent's resources, so they are overwritten with
    /// neutral values using `ptr::write` (which does not run `Drop` on the
    /// existing bytes).  The caller is responsible for installing a fresh
    /// `KernelStack` (and any other owned handle) before the child is
    /// dispatched — see `task_fork` / `task_clone`.
    pub unsafe fn clone_from_raw(&mut self, other: &Task) {
        // SAFETY: Both pointers are valid, non-overlapping Task instances.
        // The caller guarantees exclusive write access to `self`.
        //
        // The child lives in its own pool slot; the parent's slot_index
        // is irrelevant. Preserve the destination's slot_index across
        // the bulk copy so the child stays associated with the slot it
        // was reserved into.
        let preserved_slot_index = self.slot_index;
        unsafe {
            core::ptr::copy_nonoverlapping(
                other as *const Task as *const u8,
                self as *mut Task as *mut u8,
                core::mem::size_of::<Task>(),
            );
            // Neutralize bitwise-copied owned handles so their `Drop` does
            // not free resources that still belong to the parent.  Caller
            // installs real values after this returns.
            core::ptr::write(&mut self.kernel_stack as *mut _, None);
            core::ptr::write(&mut self.unsafe_stack as *mut _, None);
            self.unsafe_stack_sp = 0;
        }
        // Child keeps its own pool position.
        self.slot_index = preserved_slot_index;
        // Reset scheduler linkage and refcount — the copy is a fresh entity.
        self.next_ready = ptr::null_mut();
        self.next_inbox = AtomicPtr::new(ptr::null_mut());
        self.refcnt = AtomicU32::new(0);
        // Child inherits signal actions and blocked mask but starts with no pending signals.
        self.signal_pending = AtomicU64::new(0);
    }

    #[inline]
    pub fn inc_ref(&self) -> u32 {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == u32::MAX {
            return u32::MAX;
        }
        self.refcnt.fetch_add(1, Ordering::AcqRel) + 1
    }

    #[inline]
    pub fn dec_ref(&self) -> bool {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == 0 {
            return false;
        }
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }

    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
}
