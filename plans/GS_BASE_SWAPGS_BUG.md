# GS_BASE / SWAPGS Bug Analysis

## Status: INVESTIGATION NEEDED

**Date**: 2026-01-24  
**Severity**: Performance (not correctness - current fallback works)  
**Affected Area**: Per-CPU data access, SMP performance

---

## Executive Summary

SlopOS has infrastructure for fast GS-segment-based per-CPU data access, but it's currently disabled due to a bug where the `GS_BASE` MSR gets corrupted during user/kernel transitions. The kernel falls back to slow LAPIC MMIO reads (~100+ cycles) instead of fast `gs:[0]` access (~1-3 cycles).

**Current state**: Kernel boots and passes all tests using LAPIC fallback.  
**Goal**: Enable GS-based `get_current_cpu()` for SMP performance improvement.

---

## Problem Description

### What We Tried

Changed `get_current_cpu()` from:
```rust
// SLOW: ~100+ cycles per call
let apic_id = read_lapic_id();  // MMIO read
cpu_index_from_apic_id(apic_id)
```

To:
```rust
// FAST: ~1-3 cycles per call
let cpu_id: u32;
asm!("mov {:e}, gs:[0]", out(reg) cpu_id);
cpu_id as usize
```

### What Happened

Kernel crashed at scheduler startup. Debug output showed:
- GS_BASE correctly set to `0xffffffff8058d080` (PerCpuData) during init
- Later reads showed GS_BASE = `0xffffffff8031e0e0` (PerCpuSyscallData)
- Reading `gs:[0]` returned garbage because it was reading from wrong structure

---

## Root Cause Analysis

### The Two Per-CPU Structures

SlopOS uses two separate per-CPU data structures:

1. **`PerCpuData`** (in `lib/src/percpu.rs`)
   - General kernel per-CPU data
   - Contains: `cpu_id`, `apic_id`, `current_task`, `preempt_count`, etc.
   - Layout: `cpu_id` at offset 0

2. **`PerCpuSyscallData`** (in `boot/src/gdt.rs`)
   - SYSCALL-specific data for fast user/kernel transitions
   - Contains: `user_rsp_scratch` (offset 0), `kernel_rsp` (offset 8)
   - Used by SYSCALL entry to save user RSP and load kernel RSP

### The MSR Setup

During boot:
- `GS_BASE` (MSR 0xC0000101) = PerCpuData address
- `KERNEL_GS_BASE` (MSR 0xC0000102) = PerCpuSyscallData address

### The SWAPGS Instruction

The `swapgs` instruction atomically exchanges `GS_BASE` and `KERNEL_GS_BASE`. It's used for user/kernel transitions:

**SYSCALL entry** (`idt_handlers.s:278`):
```asm
syscall_entry:
    swapgs                    # GS_BASE <-> KERNEL_GS_BASE
    movq %rsp, %gs:0          # Save user RSP (expects PerCpuSyscallData)
    movq %gs:8, %rsp          # Load kernel RSP
```

**Interrupt entry** (`idt_handlers.s:29-31`):
```asm
testb $3, 24(%rsp)    # Check if from user mode (CS RPL bits)
jz 1f                  # Skip if from kernel
swapgs                 # Swap if from user mode
1:
```

### The Bug Sequence

1. **Kernel task runs** - `GS_BASE = PerCpuData`
2. **`context_switch_user` to user task**:
   - Sets `GS_BASE = 0` (for user mode)
   - Sets `KERNEL_GS_BASE = PerCpuSyscallData`
3. **User task runs** - `GS_BASE = 0`
4. **Timer interrupt from user mode**:
   - Entry: `swapgs` executes (from user)
   - Now: `GS_BASE = PerCpuSyscallData`, `KERNEL_GS_BASE = 0`
   - Handler runs... 
   - Exit: `swapgs` executes (returning to user)
   - Now: `GS_BASE = 0`, `KERNEL_GS_BASE = PerCpuSyscallData`
5. **User task resumes** - `GS_BASE = 0` (fine for user)
6. **Switch back to kernel task via `context_switch`** (kernel-to-kernel):
   - **BUG**: `GS_BASE` is still 0!
   - Kernel code reading `gs:[0]` gets garbage

### Why Kernel-to-Kernel Switch Doesn't Fix GS_BASE

The `context_switch` function (in `core/context_switch.s`) does NOT restore `GS_BASE`. It was designed before GS-based per-CPU access was implemented. It restores other segment selectors but deliberately skips GS because writing to the GS selector zeros the GS_BASE MSR.

---

## Current Mitigations

1. **`get_current_cpu()` uses LAPIC fallback** - Works but slow
2. **`context_switch.s` doesn't touch GS selector** - Prevents zeroing GS_BASE, but doesn't fix it either
3. **GS_BASE activation code exists but is ineffective** - `activate_gs_base_for_cpu()` only runs during boot

---

## Investigation Tasks

### Task 1: Understand the Full State Machine

Map out all GS_BASE/KERNEL_GS_BASE transitions:
- [ ] Boot sequence (BSP)
- [ ] AP startup sequence
- [ ] SYSCALL entry/exit
- [ ] Interrupt entry/exit (from user)
- [ ] Interrupt entry/exit (from kernel)
- [ ] context_switch (kernel-to-kernel)
- [ ] context_switch_user (kernel-to-user)

### Task 2: Determine Correct GS_BASE Values

For each execution context, what should GS_BASE be?
- [ ] Kernel mode: `PerCpuData[cpu_id]`
- [ ] User mode: 0 (or user-defined if supporting user TLS)
- [ ] During interrupt handler: ???
- [ ] During SYSCALL handler: ???

### Task 3: Evaluate Solution Options

#### Option A: Restore GS_BASE in `context_switch`

**Approach**: After switching to a kernel task, write the correct PerCpuData address to GS_BASE MSR.

**Challenge**: How to know which CPU we're on? Can't use `gs:[0]` (it's broken). Options:
1. Read LAPIC ID via MMIO (defeats the purpose somewhat)
2. Store PerCpuData pointer in TaskContext
3. Use `rdtscp` (returns CPU ID in ECX on some CPUs)

**Complexity**: Medium

#### Option B: Merge PerCpuSyscallData into PerCpuData

**Approach**: Use a single per-CPU structure. Rearrange fields so SYSCALL can use offsets 0 and 8.

```rust
#[repr(C, align(64))]
pub struct PerCpuData {
    pub user_rsp_scratch: u64,  // offset 0 - for SYSCALL
    pub kernel_rsp: u64,        // offset 8 - for SYSCALL
    pub cpu_id: u32,            // offset 16
    pub apic_id: u32,           // offset 20
    // ... rest of fields
}
```

Then `GS_BASE` always points to this unified structure:
- SYSCALL uses `gs:[0]` and `gs:[8]`
- Kernel uses `gs:[16]` for cpu_id

**Challenge**: Need to update all GS-based offsets, update SYSCALL handler.

**Complexity**: High (structural change)

#### Option C: Restore GS_BASE on Interrupt Exit

**Approach**: In the interrupt exit path, when returning to kernel mode after coming from user mode, explicitly restore GS_BASE.

**Challenge**: Need to detect "returning to kernel that was interrupted from user context" case. This is complex because the interrupt might have triggered a context switch.

**Complexity**: High

#### Option D: Use FSGSBASE Instructions

**Approach**: Use `rdgsbase`/`wrgsbase` instructions instead of MSR access. These are faster and may behave differently.

**Challenge**: Requires FSGSBASE CPU feature (need to check if enabled).

**Complexity**: Low if supported

### Task 4: Implement and Test

Once a solution is chosen:
- [ ] Implement the fix
- [ ] Enable GS-based `get_current_cpu()`
- [ ] Run full test suite
- [ ] Benchmark SMP performance (roulette FPS with 2+ CPUs)

---

## Files Involved

| File | Role |
|------|------|
| `lib/src/percpu.rs` | PerCpuData struct, `activate_gs_base_for_cpu()`, `get_current_cpu()` |
| `boot/src/gdt.rs` | PerCpuSyscallData, `syscall_msr_init()`, MSR setup |
| `boot/idt_handlers.s` | SYSCALL entry, interrupt macros with swapgs |
| `core/context_switch.s` | Context switch assembly, currently skips GS |
| `boot/src/smp.rs` | AP initialization, calls `activate_gs_base_for_cpu()` |

---

## Related Commits

- Initial GS_BASE infrastructure added
- `context_switch.s` modified to skip GS selector restore
- `get_current_cpu()` reverted to LAPIC fallback

---

## References

- Intel SDM Vol. 3A, Section 5.8.8 - SWAPGS instruction
- Intel SDM Vol. 3A, Section 5.8.7 - 64-Bit Mode Segment Registers
- Linux kernel `arch/x86/entry/entry_64.S` - SWAPGS usage
- Redox OS `src/arch/x86_64/interrupt/syscall.rs` - SYSCALL handling

---

## Success Criteria

1. `get_current_cpu()` uses `gs:[offset]` instead of LAPIC read
2. All 363 tests pass
3. Kernel boots and runs stably with 2+ CPUs
4. Roulette animation runs at ~83 FPS with SMP (vs ~1 FPS with current lock contention)
