# Safe Context Switch Architecture Plan

## Executive Summary

This document outlines a plan to modernize SlopOS's context switching mechanism, eliminating hardcoded assembly offsets and enabling safe modification of task-related structures. The approach draws inspiration from both Linux (build-time offset generation) and Redox OS (Rust `offset_of!` in inline assembly).

---

## Current Problem

### The Frozen ABI Issue

SlopOS's context switching is implemented in `core/context_switch.s` with **hardcoded byte offsets**:

```asm
# Current: Hardcoded offsets that MUST match Rust struct layout
.equ FPU_STATE_OFFSET, 0xD0

movq    %rbx, 0x08(%rdi)    # rbx at offset 0x08
movq    %rsp, 0x38(%rdi)    # rsp at offset 0x38
movq    %rax, 0x80(%rdi)    # rip at offset 0x80
movq    %rax, 0xC0(%rdi)    # cr3 at offset 0xC0
```

**Consequence**: Any modification to `abi/src/task.rs` (adding fields, enums, or even unused code) can shift struct layouts, causing:
- Memory corruption during context switch
- Page faults at seemingly unrelated locations
- Silent data corruption that manifests later

This blocked the SMP safety improvements in `RUST_NATIVE_SMP_SAFETY.md` (Phases 1c, 2, 3).

---

## Reference Implementations

### Linux Kernel Approach: Build-Time Offset Generation

Linux uses a clever mechanism to generate offset constants at build time:

**1. `arch/x86/kernel/asm-offsets.c`:**
```c
#include <linux/kbuild.h>
#include <linux/sched.h>

static void __used common(void)
{
    OFFSET(TASK_threadsp, task_struct, thread.sp);
    OFFSET(TASK_stack_canary, task_struct, stack_canary);
    // ... more offsets
}
```

**2. Build system compiles this and extracts constants:**
```
TASK_threadsp = 2048
TASK_stack_canary = 2056
```

**3. Assembly includes generated header:**
```asm
#include <asm/asm-offsets.h>

movq    %rsp, TASK_threadsp(%rdi)
movq    TASK_threadsp(%rsi), %rsp
```

**Linux's `__switch_to_asm` (simplified):**
```asm
SYM_FUNC_START(__switch_to_asm)
    # Save callee-saved registers (order matches inactive_task_frame)
    pushq   %rbp
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15

    # Switch stack using generated offset
    movq    %rsp, TASK_threadsp(%rdi)
    movq    TASK_threadsp(%rsi), %rsp

    # Restore callee-saved registers
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    popq    %rbp

    jmp     __switch_to
SYM_FUNC_END(__switch_to_asm)
```

**Key Insight**: Linux only saves/restores **callee-saved registers** in assembly. Everything else (FPU, segments, CR3) is handled in C code (`__switch_to`).

---

### Redox OS Approach: Rust `offset_of!` in Inline Assembly

Redox OS eliminates external assembly files entirely, using Rust's `core::mem::offset_of!` macro:

**`src/context/arch/x86_64.rs`:**
```rust
use core::mem::offset_of;

#[repr(C)]
pub struct Context {
    rflags: usize,
    rbx: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
    rbp: usize,
    pub(crate) rsp: usize,
    pub(crate) fsbase: usize,
    pub(crate) gsbase: usize,
    userspace_io_allowed: bool,
}

#[unsafe(naked)]
unsafe extern "sysv64" fn switch_to_inner(_prev: &mut Context, _next: &mut Context) {
    use Context as Cx;

    core::arch::naked_asm!(
        concat!("
        // Save old registers, load new ones
        mov [rdi + {off_rbx}], rbx
        mov rbx, [rsi + {off_rbx}]

        mov [rdi + {off_r12}], r12
        mov r12, [rsi + {off_r12}]

        mov [rdi + {off_r13}], r13
        mov r13, [rsi + {off_r13}]

        mov [rdi + {off_r14}], r14
        mov r14, [rsi + {off_r14}]

        mov [rdi + {off_r15}], r15
        mov r15, [rsi + {off_r15}]

        mov [rdi + {off_rbp}], rbp
        mov rbp, [rsi + {off_rbp}]

        mov [rdi + {off_rsp}], rsp
        mov rsp, [rsi + {off_rsp}]

        // RFLAGS via stack
        pushfq
        pop QWORD PTR [rdi + {off_rflags}]
        push QWORD PTR [rsi + {off_rflags}]
        popfq

        jmp {switch_hook}
        "),

        off_rflags = const(offset_of!(Cx, rflags)),
        off_rbx = const(offset_of!(Cx, rbx)),
        off_r12 = const(offset_of!(Cx, r12)),
        off_r13 = const(offset_of!(Cx, r13)),
        off_r14 = const(offset_of!(Cx, r14)),
        off_r15 = const(offset_of!(Cx, r15)),
        off_rbp = const(offset_of!(Cx, rbp)),
        off_rsp = const(offset_of!(Cx, rsp)),

        switch_hook = sym crate::context::switch_finish_hook,
    );
}
```

**Key Insight**: Redox uses `const` expressions in `naked_asm!` to embed offsets directly. The compiler computes offsets at compile time, ensuring they always match the struct layout.

**FPU/FSGSBASE Handling (in Rust, before calling `switch_to_inner`):**
```rust
pub unsafe fn switch_to(prev: &mut super::Context, next: &mut super::Context) {
    // FPU state - handled in Rust with inline asm
    core::arch::asm!(
        "fxsave64 [{prev_fx}]",
        "fxrstor64 [{next_fx}]",
        prev_fx = in(reg) prev.kfx.as_mut_ptr(),
        next_fx = in(reg) next.kfx.as_ptr(),
    );

    // FSBASE/GSBASE - handled in Rust
    core::arch::asm!(
        "mov rax, [{next}+{fsbase_off}]",
        "wrfsbase rax",
        // ... more
        fsbase_off = const offset_of!(Context, fsbase),
        // ...
    );

    // Finally, switch registers
    switch_to_inner(&mut prev.arch, &mut next.arch)
}
```

---

## Proposed Architecture for SlopOS

### Design Principles

1. **Minimal Assembly**: Only save/restore callee-saved registers in naked assembly
2. **Offset Safety**: Use `offset_of!` for all struct field access
3. **Separation of Concerns**: FPU, CR3, segments handled in Rust before/after core switch
4. **SMP Safety**: Context switch lock + memory barriers
5. **Type Safety**: Enable future TaskStatus/BlockReason enums without ABI breakage

### Implementation Phases

---

## Phase 1: Create Minimal Context Struct

**Goal**: Separate "switch context" (registers) from "task metadata" (state, priority, etc.)

**New file: `core/src/scheduler/switch_context.rs`**

```rust
use core::mem::offset_of;

/// Minimal CPU context for context switching.
/// Contains ONLY what must be saved/restored in assembly.
/// All other task state lives in Task struct.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SwitchContext {
    // Callee-saved registers (System V ABI)
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rflags: u64,
    // Instruction pointer (for initial setup)
    pub rip: u64,
}

impl SwitchContext {
    pub const fn zero() -> Self {
        Self {
            rbx: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            rbp: 0, rsp: 0, rflags: 0x202, rip: 0,
        }
    }

    /// Setup for a new task
    pub fn setup_initial(&mut self, stack_top: u64, entry_point: u64) {
        self.rsp = stack_top;
        self.rip = entry_point;
        self.rflags = 0x202; // IF=1
    }
}

// Export offsets for use in assembly
pub mod offsets {
    use super::*;

    pub const RBX: usize = offset_of!(SwitchContext, rbx);
    pub const R12: usize = offset_of!(SwitchContext, r12);
    pub const R13: usize = offset_of!(SwitchContext, r13);
    pub const R14: usize = offset_of!(SwitchContext, r14);
    pub const R15: usize = offset_of!(SwitchContext, r15);
    pub const RBP: usize = offset_of!(SwitchContext, rbp);
    pub const RSP: usize = offset_of!(SwitchContext, rsp);
    pub const RFLAGS: usize = offset_of!(SwitchContext, rflags);
    pub const RIP: usize = offset_of!(SwitchContext, rip);
}
```

---

## Phase 2: Rewrite Context Switch in Rust Inline Assembly

**Goal**: Eliminate `context_switch.s`, use `naked_asm!` with `offset_of!`

**New file: `core/src/scheduler/switch_asm.rs`**

```rust
use core::arch::naked_asm;
use core::mem::offset_of;
use super::switch_context::SwitchContext;

/// Low-level register switch. Only touches callee-saved registers.
/// 
/// # Safety
/// - Both contexts must be valid and properly initialized
/// - Must be called with interrupts disabled
/// - Must not be called recursively on the same CPU
#[unsafe(naked)]
pub unsafe extern "sysv64" fn switch_registers(
    prev: *mut SwitchContext,
    next: *const SwitchContext,
) {
    // rdi = prev, rsi = next
    naked_asm!(
        // Save callee-saved registers to prev context
        "mov [rdi + {off_rbx}], rbx",
        "mov [rdi + {off_r12}], r12",
        "mov [rdi + {off_r13}], r13",
        "mov [rdi + {off_r14}], r14",
        "mov [rdi + {off_r15}], r15",
        "mov [rdi + {off_rbp}], rbp",
        "mov [rdi + {off_rsp}], rsp",

        // Save RFLAGS
        "pushfq",
        "pop QWORD PTR [rdi + {off_rflags}]",

        // Save return address as RIP (for debugging/initial setup)
        "mov rax, [rsp]",
        "mov [rdi + {off_rip}], rax",

        // Load callee-saved registers from next context
        "mov rbx, [rsi + {off_rbx}]",
        "mov r12, [rsi + {off_r12}]",
        "mov r13, [rsi + {off_r13}]",
        "mov r14, [rsi + {off_r14}]",
        "mov r15, [rsi + {off_r15}]",
        "mov rbp, [rsi + {off_rbp}]",

        // Load RFLAGS
        "push QWORD PTR [rsi + {off_rflags}]",
        "popfq",

        // Switch stack
        "mov rsp, [rsi + {off_rsp}]",

        // Return (pops return address from new stack)
        "ret",

        off_rbx = const offset_of!(SwitchContext, rbx),
        off_r12 = const offset_of!(SwitchContext, r12),
        off_r13 = const offset_of!(SwitchContext, r13),
        off_r14 = const offset_of!(SwitchContext, r14),
        off_r15 = const offset_of!(SwitchContext, r15),
        off_rbp = const offset_of!(SwitchContext, rbp),
        off_rsp = const offset_of!(SwitchContext, rsp),
        off_rflags = const offset_of!(SwitchContext, rflags),
        off_rip = const offset_of!(SwitchContext, rip),
    );
}

/// Entry point for new tasks. Called when a task runs for the first time.
#[unsafe(naked)]
pub unsafe extern "sysv64" fn task_entry_trampoline() {
    naked_asm!(
        // New task's entry point is in r12, argument in r13
        // (set up by task creation code)
        "mov rdi, r13",  // arg -> first parameter
        "call r12",      // call entry point
        
        // If entry returns, call task exit
        "call {task_exit}",
        
        // Should never reach here
        "ud2",
        
        task_exit = sym crate::scheduler::scheduler_task_exit_impl,
    );
}
```

---

## Phase 3: High-Level Context Switch in Safe Rust

**Goal**: Handle FPU, CR3, segments, and SMP synchronization in safe(r) Rust

**Modify: `core/src/scheduler/scheduler.rs`**

```rust
use super::switch_asm::switch_registers;
use super::switch_context::SwitchContext;
use core::sync::atomic::{fence, Ordering};

/// Perform a full context switch between two tasks.
/// 
/// This function:
/// 1. Saves/restores FPU state
/// 2. Switches page tables (CR3)
/// 3. Updates TSS for kernel stack
/// 4. Performs the actual register switch
pub unsafe fn do_context_switch(prev: &mut Task, next: &mut Task) {
    // Memory barrier before switch
    fence(Ordering::SeqCst);

    // Save FPU state for prev task
    if prev.flags & TASK_FLAG_FPU_INITIALIZED != 0 {
        save_fpu_state(&mut prev.fpu_state);
    }

    // Switch page tables if needed
    let prev_cr3 = read_cr3();
    let next_cr3 = if next.process_id != INVALID_PROCESS_ID {
        let page_dir = process_vm_get_page_dir(next.process_id);
        if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
            (*page_dir).pml4_phys.as_u64()
        } else {
            prev_cr3
        }
    } else {
        paging_get_kernel_directory_phys()
    };

    if next_cr3 != prev_cr3 {
        write_cr3(next_cr3);
    }

    // Update TSS kernel stack for ring transitions
    if next.flags & TASK_FLAG_USER_MODE != 0 {
        platform::gdt_set_kernel_rsp0(next.kernel_stack_top);
    }

    // Restore FPU state for next task
    if next.flags & TASK_FLAG_FPU_INITIALIZED != 0 {
        restore_fpu_state(&next.fpu_state);
    }

    // Perform the actual register switch
    switch_registers(&mut prev.switch_ctx, &next.switch_ctx);

    // Memory barrier after switch
    fence(Ordering::SeqCst);
}

#[inline(always)]
unsafe fn save_fpu_state(state: &mut FpuState) {
    core::arch::asm!(
        "fxsave64 [{}]",
        in(reg) state.as_mut_ptr(),
        options(nostack, preserves_flags)
    );
}

#[inline(always)]
unsafe fn restore_fpu_state(state: &FpuState) {
    core::arch::asm!(
        "fxrstor64 [{}]",
        in(reg) state.as_ptr(),
        options(nostack, preserves_flags)
    );
}
```

---

## Phase 4: Update Task Struct

**Goal**: Replace old TaskContext with new SwitchContext

**Modify: `abi/src/task.rs`**

```rust
// OLD (remove):
pub struct TaskContext {
    pub rax: u64,
    pub rbx: u64,
    // ... 25 fields
}

// NEW (add in core, reference in abi):
// The SwitchContext is defined in core/src/scheduler/switch_context.rs
// Task struct just holds a reference/embedded copy

#[repr(C)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    pub state: u8,
    pub priority: u8,
    pub flags: u16,
    pub process_id: u32,

    // Stack info
    pub stack_base: u64,
    pub stack_size: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,

    // Entry point
    pub entry_point: u64,
    pub entry_arg: *mut c_void,

    // NEW: Minimal switch context (replaces TaskContext)
    pub switch_ctx: SwitchContext,

    // FPU state (kept separate, handled in Rust)
    pub fpu_state: FpuState,

    // ... rest of fields unchanged
}
```

---

## Phase 5: SMP-Safe Switch Lock

**Goal**: Ensure atomic context switches across CPUs (like Redox's CONTEXT_SWITCH_LOCK)

```rust
use core::sync::atomic::{AtomicBool, Ordering};

/// Global lock for context switches.
/// Prevents races when multiple CPUs try to switch simultaneously.
static CONTEXT_SWITCH_LOCK: AtomicBool = AtomicBool::new(false);

pub fn acquire_switch_lock() {
    while CONTEXT_SWITCH_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

pub fn release_switch_lock() {
    CONTEXT_SWITCH_LOCK.store(false, Ordering::SeqCst);
}

/// Full context switch with proper locking
pub unsafe fn context_switch_locked(prev: &mut Task, next: &mut Task) {
    acquire_switch_lock();
    
    do_context_switch(prev, next);
    
    release_switch_lock();
}
```

---

## Phase 6: User Mode Support

**Goal**: Handle syscall/sysret transitions properly

```rust
/// Context switch that returns to user mode
#[unsafe(naked)]
pub unsafe extern "sysv64" fn switch_to_user(
    prev_ctx: *mut SwitchContext,
    user_regs: *const UserRegs,
) {
    naked_asm!(
        // Save kernel context
        "mov [rdi + {off_rbx}], rbx",
        // ... save other callee-saved regs

        // Load user registers from UserRegs struct
        // Then sysretq or iretq to user mode
        // ...

        off_rbx = const offset_of!(SwitchContext, rbx),
        // ...
    );
}
```

---

## Migration Plan

### Step 1: Add New Code Alongside Old
- Create `switch_context.rs` and `switch_asm.rs`
- Keep existing `context_switch.s` working
- Add feature flag `new_context_switch`

### Step 2: Parallel Testing
- Run tests with both implementations
- Compare behavior under SMP stress

### Step 3: Gradual Migration
- Switch one code path at a time
- Start with kernel-to-kernel switches
- Then kernel-to-user switches

### Step 4: Remove Old Code
- Delete `context_switch.s`
- Remove old `TaskContext` struct
- Update all references

---

## Benefits After Migration

### Enables Previously Blocked Features

| Feature | Why It's Now Possible |
|---------|----------------------|
| `TaskStatus` enum | Can modify `abi/src/task.rs` freely |
| `BlockReason` enum | No hardcoded offsets to break |
| `task_find_by_id_ref()` | Safe to add new functions |
| Type-safe state transitions | Full Phase 2/3 from SMP plan |

### Improved Safety

| Aspect | Improvement |
|--------|-------------|
| Compile-time checks | `offset_of!` catches layout changes |
| Smaller unsafe surface | Only `switch_registers` is naked asm |
| Memory barriers | Explicit in Rust, not hidden in asm |
| Debugging | Rust code is easier to debug than asm |

### Maintenance

| Aspect | Improvement |
|--------|-------------|
| Adding task fields | Just add to struct, offsets auto-update |
| Platform ports | Easier to port to aarch64/riscv64 |
| Code review | More code in Rust = more reviewable |

---

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Performance regression | Benchmark before/after; naked_asm is same perf as .s |
| Subtle bugs during migration | Extensive testing with `make test`; parallel implementations |
| User mode edge cases | Test WIN path specifically (known issue) |
| Nightly Rust dependency | Already using nightly; `naked_asm` is stabilizing |

---

## References

- [Linux kernel context switching](https://github.com/torvalds/linux/blob/master/arch/x86/entry/entry_64.S)
- [Linux asm-offsets mechanism](https://github.com/torvalds/linux/blob/master/arch/x86/kernel/asm-offsets.c)
- [Redox OS context switching](https://gitlab.redox-os.org/redox-os/kernel/-/blob/master/src/context/arch/x86_64.rs)
- [Rust `offset_of!` RFC](https://rust-lang.github.io/rfcs/3308-offset_of.html)
- [Rust Atomics and Locks (Mara Bos)](https://marabos.nl/atomics/)

---

## Phase Dependencies

| Phase | Dependencies |
|-------|--------------|
| Phase 1: SwitchContext | None |
| Phase 2: Rust inline asm | Phase 1 |
| Phase 3: High-level switch | Phase 2 |
| Phase 4: Update Task | Phase 3 |
| Phase 5: SMP lock | Phase 4 |
| Phase 6: User mode | Phase 5 |
| Testing & migration | All phases |

---

## Success Criteria

1. `make test` passes with 358+ tests
2. WIN path no longer crashes (verify with `make boot VIDEO=1`)
3. Can add `TaskStatus` enum to `abi/src/task.rs` without test failures
4. SMP stress tests pass (multiple CPUs, rapid context switches)
5. No performance regression in context switch latency
