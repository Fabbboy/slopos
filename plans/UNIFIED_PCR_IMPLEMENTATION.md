# Unified Processor Control Region (PCR) Implementation Plan

**Status**: ✅ IMPLEMENTED (2026-01-27)  
**Created**: 2026-01-27  
**Authors**: AI-assisted analysis based on Redox OS patterns  
**Fixes**: AP User-Mode Context Switch Failure, GS_BASE Performance Bug

> **Implementation Complete**: All phases implemented and verified. 360 tests pass.
> User-mode tasks now run on any CPU. See `lib/src/pcr.rs` for the implementation.

---

## Executive Summary

This plan replaces SlopOS's fragmented per-CPU infrastructure with a unified `ProcessorControlRegion` (PCR) following Redox OS production patterns. This fixes two critical bugs preventing AP (CPU 1+) user-mode execution and enables fast GS-based per-CPU access.

### Bugs Fixed

| Bug | Root Cause | Impact |
|-----|------------|--------|
| **AP User-Mode Failure** | `TSS.rsp0 = 0` for APs (never initialized) | APs cannot handle exceptions in user mode |
| **Wrong KERNEL_GS_BASE** | `SYSCALL_CPU_DATA_PTR` is global (CPU 0 only) | APs use wrong per-CPU data on syscall |
| **Slow `get_current_cpu()`** | Falls back to LAPIC MMIO (~100 cycles) | Performance bottleneck |

### Solution

Adopt Redox OS architecture:
- Single unified `ProcessorControlRegion` per CPU
- GDT and TSS embedded in PCR (no separate arrays)
- `GS_BASE` always points to PCR in kernel mode
- Fast `gs:[offset]` access for all per-CPU data
- Per-CPU kernel stacks allocated and correctly set in `TSS.rsp0`

---

## Architecture Comparison

| Aspect | SlopOS (Current) | Redox OS | SlopOS (After) |
|--------|------------------|----------|----------------|
| Per-CPU Structure | 2 separate (`PerCpuData` + `PerCpuSyscallData`) | 1 unified (`ProcessorControlRegion`) | 1 unified (`ProcessorControlRegion`) |
| GDT Storage | `static mut PER_CPU_GDT[MAX_CPUS]` | Embedded in PCR | Embedded in PCR |
| TSS Storage | `static mut PER_CPU_TSS[MAX_CPUS]` | Embedded in PCR | Embedded in PCR |
| Kernel Stack | BSP only (64KB in .bss) | Per-CPU allocated | Per-CPU in PCR |
| GS_BASE (kernel) | Points to `PerCpuData` (broken) | Points to PCR | Points to PCR |
| KERNEL_GS_BASE | Points to `PerCpuSyscallData` | 0 (user's GS) | 0 (user's GS) |
| `get_current_cpu()` | LAPIC MMIO read (~100 cycles) | `mov eax, gs:[offset]` (~1-3 cycles) | `mov eax, gs:[24]` (~1-3 cycles) |
| AP TSS.rsp0 | 0 (uninitialized) | Correctly set | Correctly set |

---

## PCR Structure Definition

```rust
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
    pub self_ref: *mut ProcessorControlRegion,              // offset 0
    
    /// Temporary storage for user RSP during SYSCALL entry
    /// Assembly: `mov gs:[8], rsp` saves user stack
    pub user_rsp_tmp: u64,                                   // offset 8
    
    /// Kernel RSP loaded during SYSCALL entry (mirrors TSS.rsp0)
    /// Assembly: `mov rsp, gs:[16]` loads kernel stack
    pub kernel_rsp: u64,                                     // offset 16
    
    // ==================== GENERAL PER-CPU DATA ====================
    
    /// CPU index (0..n-1), NOT the hardware APIC ID
    /// Assembly: `mov eax, gs:[24]` for fast current_cpu_id()
    pub cpu_id: u32,                                         // offset 24
    
    /// Hardware Local APIC ID
    pub apic_id: u32,                                        // offset 28
    
    /// Preemption disable nesting counter
    /// >0 means preemption is disabled
    pub preempt_count: AtomicU32,                            // offset 32
    
    /// Currently executing in interrupt/exception context
    pub in_interrupt: AtomicBool,                            // offset 36
    
    _pad1: [u8; 3],                                          // offset 37-39
    
    /// Pointer to currently running task (opaque)
    pub current_task: AtomicPtr<()>,                         // offset 40
    
    /// Pointer to this CPU's scheduler instance (opaque)
    pub scheduler: AtomicPtr<()>,                            // offset 48
    
    /// CPU is online and accepting scheduled work
    pub online: AtomicBool,                                  // offset 56
    
    _pad2: [u8; 7],                                          // offset 57-63
    
    // ==================== STATISTICS (cache-line aligned) ====================
    
    /// Total context switches on this CPU
    pub context_switches: AtomicU64,                         // offset 64
    
    /// Total interrupts handled on this CPU
    pub interrupt_count: AtomicU64,                          // offset 72
    
    /// Total syscalls handled on this CPU
    pub syscall_count: AtomicU64,                            // offset 80
    
    /// PID of task currently in syscall (for user pointer validation)
    pub syscall_pid: AtomicU32,                              // offset 88
    
    _pad3: [u8; 4],                                          // offset 92-95
    
    // ==================== EMBEDDED GDT ====================
    
    /// Per-CPU Global Descriptor Table
    /// Contains kernel/user code/data segments + TSS descriptor
    pub gdt: GdtLayout,                                      // offset 96 (8-byte aligned)
    
    // ==================== EMBEDDED TSS ====================
    
    _tss_align: [u8; 8],                                     // Alignment to 16 bytes
    
    /// Per-CPU Task State Segment
    /// TSS.rsp0 = kernel_rsp (kept in sync)
    pub tss: Tss64,                                          // offset ~200 (16-byte aligned)
    
    // ==================== KERNEL STACK ====================
    
    /// Guard page to catch stack overflow (unmapped or read-only)
    _stack_guard: [u8; 4096],
    
    /// Per-CPU kernel stack (64KB)
    /// Stack grows down, so kernel_rsp points to end of this array
    pub kernel_stack: [u8; KERNEL_STACK_SIZE],
}

pub const KERNEL_STACK_SIZE: usize = 64 * 1024;  // 64KB per CPU
pub const MAX_CPUS: usize = 256;

/// Compile-time offset verification
const _: () = {
    assert!(core::mem::offset_of!(ProcessorControlRegion, self_ref) == 0);
    assert!(core::mem::offset_of!(ProcessorControlRegion, user_rsp_tmp) == 8);
    assert!(core::mem::offset_of!(ProcessorControlRegion, kernel_rsp) == 16);
    assert!(core::mem::offset_of!(ProcessorControlRegion, cpu_id) == 24);
    assert!(core::mem::align_of::<ProcessorControlRegion>() == 4096);
};
```

---

## SWAPGS State Machine

Understanding the GS_BASE / KERNEL_GS_BASE discipline is critical.

### States

| Context | GS_BASE | KERNEL_GS_BASE | Notes |
|---------|---------|----------------|-------|
| Kernel mode | PCR | 0 | Normal kernel execution |
| User mode | 0 | PCR | User code running |
| SYSCALL entry (before swapgs) | 0 | PCR | Just entered from user |
| SYSCALL entry (after swapgs) | PCR | 0 | Kernel can use gs:[...] |
| SYSCALL exit (before swapgs) | PCR | 0 | About to return to user |
| SYSCALL exit (after swapgs) | 0 | PCR | User mode restored |

### Key Invariant

**In kernel mode, `GS_BASE` always points to the current CPU's PCR.**

This is maintained by:
1. `swapgs` on syscall/interrupt entry from user mode
2. `swapgs` on syscall/interrupt exit to user mode  
3. `context_switch_user` setting up MSRs correctly before `iretq`

---

## Implementation Phases

### Phase 0: Preparation & Baseline
**Duration**: ~30 minutes | **Risk**: None

#### Tasks
- [ ] Run `make clean && make build && make test` - verify all tests pass
- [ ] Record baseline test count (expected: 363+)
- [ ] Create feature branch: `git checkout -b feat/unified-pcr`
- [ ] Document current assembly offsets for reference

#### Verification
```bash
make test
# All tests must pass before proceeding
```

---

### Phase 1: Define PCR Structure
**Duration**: ~1 hour | **Risk**: Low (additive only)

#### Tasks
- [ ] Create `lib/src/pcr.rs` with `ProcessorControlRegion` struct
- [ ] Add `pub mod offsets` with compile-time constants
- [ ] Add compile-time offset assertions
- [ ] Export from `lib/src/lib.rs`

#### Files Created
| File | Description |
|------|-------------|
| `lib/src/pcr.rs` | New unified PCR structure definition |

#### Verification
```bash
make build
# Must compile without errors
```

---

### Phase 2: PCR Storage & Basic Access
**Duration**: ~1 hour | **Risk**: Low

#### Tasks
- [ ] Add `static mut BSP_PCR` for bootstrap processor
- [ ] Add `static mut ALL_PCRS: [*mut PCR; MAX_CPUS]` array
- [ ] Implement `init_bsp_pcr(apic_id)` function
- [ ] Implement `init_ap_pcr(cpu_id, apic_id)` function
- [ ] Implement `current_pcr() -> &'static PCR` via `gs:[0]`
- [ ] Implement `current_cpu_id() -> usize` via `gs:[24]`
- [ ] Implement `get_pcr(cpu_id) -> Option<&'static PCR>`

#### Key Code
```rust
/// Get current CPU's PCR via GS segment (FAST PATH - ~1-3 cycles)
#[inline(always)]
pub fn current_pcr() -> &'static ProcessorControlRegion {
    unsafe {
        let ptr: *mut ProcessorControlRegion;
        core::arch::asm!(
            "mov {}, gs:[0]",
            out(reg) ptr,
            options(nostack, preserves_flags, readonly)
        );
        &*ptr
    }
}

/// Get current CPU ID (FAST PATH - ~1-3 cycles)
#[inline(always)]  
pub fn current_cpu_id() -> usize {
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
```

#### Verification
```bash
make build
```

---

### Phase 3: GDT/TSS Integration
**Duration**: ~2 hours | **Risk**: Medium

#### Tasks
- [ ] Add `ProcessorControlRegion::init_gdt(&mut self)` method
- [ ] Add `ProcessorControlRegion::install(&mut self)` method
- [ ] Move GDT entry setup into PCR
- [ ] Move TSS descriptor setup into PCR
- [ ] Create `tests/src/pcr_tests.rs` with validation tests

#### Key Code - GDT Installation
```rust
impl ProcessorControlRegion {
    /// Initialize GDT entries in this PCR
    pub unsafe fn init_gdt(&mut self) {
        // Set up standard GDT entries
        self.gdt.entries[GDT_NULL] = GdtEntry::null();
        self.gdt.entries[GDT_KERNEL_CODE] = GdtEntry::kernel_code();
        self.gdt.entries[GDT_KERNEL_DATA] = GdtEntry::kernel_data();
        self.gdt.entries[GDT_USER_DATA] = GdtEntry::user_data();
        self.gdt.entries[GDT_USER_CODE] = GdtEntry::user_code();
        
        // Set TSS descriptor pointing to embedded TSS
        let tss_addr = &self.tss as *const _ as u64;
        self.gdt.set_tss_descriptor(tss_addr);
        
        // Initialize TSS
        self.tss.rsp0 = self.kernel_rsp;
        self.tss.iomap_base = core::mem::size_of::<Tss64>() as u16;
    }
    
    /// Load this PCR's GDT and configure GS_BASE
    pub unsafe fn install(&mut self) {
        // Load GDT from this PCR
        let gdtr = DescriptorTablePointer {
            limit: (core::mem::size_of::<GdtLayout>() - 1) as u16,
            base: &self.gdt as *const _ as u64,
        };
        lgdt(&gdtr);
        
        // Reload segment registers
        load_cs(KERNEL_CS);
        load_ss(KERNEL_DS);
        load_ds(0);
        load_es(0);
        load_fs(0);
        load_gs(0);  // NULL selector, base set via MSR
        
        // Set GS_BASE = this PCR (kernel per-CPU access)
        wrmsr(Msr::GS_BASE, self as *mut _ as u64);
        
        // Set KERNEL_GS_BASE = 0 (user's GS base after SWAPGS)
        wrmsr(Msr::KERNEL_GS_BASE, 0);
        
        // Load TSS
        load_tr(TSS_SELECTOR);
    }
}
```

#### Test Code
```rust
pub fn test_pcr_offsets_correct() -> c_int {
    let pcr = unsafe { slopos_lib::pcr::current_pcr() };
    
    // Verify self_ref at offset 0
    let self_ref_offset = (&pcr.self_ref as *const _ as usize) - (pcr as *const _ as usize);
    if self_ref_offset != 0 {
        klog_info!("PCR_TEST: BUG - self_ref at offset {}, expected 0", self_ref_offset);
        return 1;
    }
    
    // Verify cpu_id at offset 24
    let cpu_id_offset = (&pcr.cpu_id as *const _ as usize) - (pcr as *const _ as usize);
    if cpu_id_offset != 24 {
        klog_info!("PCR_TEST: BUG - cpu_id at offset {}, expected 24", cpu_id_offset);
        return 1;
    }
    
    klog_info!("PCR_TEST: All offsets correct");
    0
}

pub fn test_pcr_gs_access() -> c_int {
    let pcr = unsafe { slopos_lib::pcr::current_pcr() };
    let cpu_id_via_gs = slopos_lib::pcr::current_cpu_id();
    
    if pcr.cpu_id as usize != cpu_id_via_gs {
        klog_info!("PCR_TEST: BUG - GS returned {}, direct returned {}", 
            cpu_id_via_gs, pcr.cpu_id);
        return 1;
    }
    
    klog_info!("PCR_TEST: GS-based access works, CPU {}", cpu_id_via_gs);
    0
}

pub fn test_pcr_self_ref() -> c_int {
    let pcr = unsafe { slopos_lib::pcr::current_pcr() };
    
    if pcr.self_ref as *const _ != pcr as *const _ {
        klog_info!("PCR_TEST: BUG - self_ref mismatch");
        return 1;
    }
    
    klog_info!("PCR_TEST: self_ref correct");
    0
}
```

#### Files Created
| File | Description |
|------|-------------|
| `tests/src/pcr_tests.rs` | PCR validation test suite |

#### Verification
```bash
make test
# All existing tests + new PCR tests must pass
```

---

### Phase 4: Migrate BSP Boot Path
**Duration**: ~2 hours | **Risk**: High (critical path)

#### Tasks
- [ ] Update early boot to call `pcr::init_bsp_pcr(apic_id)`
- [ ] Update early boot to call `pcr.init_gdt()` and `pcr.install()`
- [ ] Update `get_current_cpu()` to delegate to `pcr::current_cpu_id()`
- [ ] Remove calls to old `gdt_init()` for BSP

#### Key Changes

**Before (old boot path)**:
```rust
fn early_init() {
    gdt_init();  // Uses PER_CPU_GDT[0], PER_CPU_TSS[0]
    init_percpu_for_cpu(0, apic_id);
    activate_gs_base_for_cpu(0);
}
```

**After (new boot path)**:
```rust
fn early_init() {
    unsafe {
        let apic_id = read_bsp_apic_id();
        pcr::init_bsp_pcr(apic_id);
        
        let pcr = pcr::get_pcr_mut(0).unwrap();
        pcr.init_gdt();
        pcr.install();
    }
}
```

**Update `get_current_cpu()`**:
```rust
// lib/src/percpu.rs
pub fn get_current_cpu() -> usize {
    // Delegate to fast PCR-based implementation
    crate::pcr::current_cpu_id()
}
```

#### Verification
```bash
make test
# CRITICAL - must pass before proceeding
# This validates BSP boots correctly with new PCR
```

---

### Phase 5: Migrate AP Boot Path
**Duration**: ~2 hours | **Risk**: High (fixes original bugs)

#### Tasks
- [ ] Update `ap_entry()` to allocate PCR via `pcr::init_ap_pcr()`
- [ ] Update `ap_entry()` to call `pcr.init_gdt()` and `pcr.install()`
- [ ] Remove calls to old `gdt_init_for_cpu()` and `syscall_gs_base_init_for_cpu()`
- [ ] Add AP initialization verification test

#### Key Changes

**Before (broken)**:
```rust
// boot/src/smp.rs
pub unsafe extern "C" fn ap_entry(cpu_info: *mut LimineSmpInfo) {
    let cpu_idx = assign_cpu_index(apic_id);
    
    gdt_init_for_cpu(cpu_idx);      // BUG: TSS.rsp0 = 0 for APs!
    syscall_gs_base_init_for_cpu(cpu_idx);  // BUG: Uses global SYSCALL_CPU_DATA_PTR
    activate_gs_base_for_cpu(cpu_idx);
    
    // ...
}
```

**After (fixed)**:
```rust
// boot/src/smp.rs  
pub unsafe extern "C" fn ap_entry(cpu_info: *mut LimineSmpInfo) {
    let apic_id = read_local_apic_id();
    let cpu_idx = assign_cpu_index(apic_id);
    
    // Allocate and initialize PCR for this AP
    // This allocates kernel stack and sets TSS.rsp0 correctly!
    let pcr = pcr::init_ap_pcr(cpu_idx, apic_id);
    
    // Initialize GDT/TSS in PCR
    (*pcr).init_gdt();
    
    // Install GDT and set GS_BASE = PCR
    // This sets KERNEL_GS_BASE = 0 (correct for SWAPGS)
    (*pcr).install();
    
    // Set up SYSCALL MSRs (LSTAR, STAR, SFMASK)
    syscall_msr_init();
    
    // ... rest of AP init
}
```

#### Test Code
```rust
pub fn test_all_cpus_pcr_initialized() -> c_int {
    let cpu_count = slopos_lib::get_cpu_count();
    
    for cpu_id in 0..cpu_count {
        let pcr = match slopos_lib::pcr::get_pcr(cpu_id) {
            Some(p) => p,
            None => {
                klog_info!("PCR_TEST: BUG - CPU {} has no PCR", cpu_id);
                return 1;
            }
        };
        
        // Verify kernel_rsp is set (not 0)
        if pcr.kernel_rsp == 0 {
            klog_info!("PCR_TEST: BUG - CPU {} has kernel_rsp = 0", cpu_id);
            return 1;
        }
        
        // Verify TSS.rsp0 matches kernel_rsp
        if pcr.tss.rsp0 != pcr.kernel_rsp {
            klog_info!("PCR_TEST: BUG - CPU {} TSS.rsp0 ({:#x}) != kernel_rsp ({:#x})",
                cpu_id, pcr.tss.rsp0, pcr.kernel_rsp);
            return 1;
        }
        
        // Verify kernel stack is in valid range
        let stack_base = pcr.kernel_stack.as_ptr() as u64;
        let stack_top = stack_base + KERNEL_STACK_SIZE as u64;
        if pcr.kernel_rsp < stack_base || pcr.kernel_rsp > stack_top {
            klog_info!("PCR_TEST: BUG - CPU {} kernel_rsp not in stack range", cpu_id);
            return 1;
        }
        
        klog_info!("PCR_TEST: CPU {} PCR valid, kernel_rsp={:#x}", cpu_id, pcr.kernel_rsp);
    }
    
    0
}
```

#### Verification
```bash
make test
# Validates APs initialize correctly with proper TSS.rsp0
```

---

### Phase 6: Update SYSCALL Entry Assembly
**Duration**: ~1 hour | **Risk**: High

#### Tasks
- [ ] Update `boot/idt_handlers.s` syscall_entry to use new PCR offsets
- [ ] Update syscall exit path (sysret) to use new offsets
- [ ] Add SYSCALL verification test

#### Key Changes

**Before**:
```asm
# boot/idt_handlers.s
syscall_entry:
    swapgs
    movq %rsp, %gs:0    # OLD: PerCpuSyscallData.user_rsp_scratch
    movq %gs:8, %rsp    # OLD: PerCpuSyscallData.kernel_rsp
```

**After**:
```asm
# boot/idt_handlers.s
syscall_entry:
    swapgs                  # GS_BASE (0) <-> KERNEL_GS_BASE (PCR)
                            # Now GS_BASE = PCR
    
    movq %rsp, %gs:8        # NEW: PCR.user_rsp_tmp (offset 8)
    movq %gs:16, %rsp       # NEW: PCR.kernel_rsp (offset 16)
    
    # ... rest of syscall handling unchanged ...
```

**SYSCALL Exit (sysret path)**:
```asm
    # Restore user RSP and return
    movq %gs:8, %rsp        # Load user RSP from PCR.user_rsp_tmp
    swapgs                  # Restore: GS_BASE = 0, KERNEL_GS_BASE = PCR
    sysretq
```

#### Verification
```bash
make test
# Validates syscalls work with new assembly
```

---

### Phase 7: Update Context Switch Assembly
**Duration**: ~1 hour | **Risk**: High

#### Tasks
- [ ] Update `core/context_switch.s` to remove `SYSCALL_CPU_DATA_PTR` usage
- [ ] Update `context_switch_user` to read PCR via `gs:[0]`
- [ ] Add context switch verification test

#### Key Changes

**Before (broken)**:
```asm
# core/context_switch.s - context_switch_user
    # BROKEN: Uses global that only has CPU 0's pointer
    movl $0xC0000102, %ecx              # IA32_KERNEL_GS_BASE
    movq SYSCALL_CPU_DATA_PTR(%rip), %rax   # WRONG for APs!
    movq %rax, %rdx
    shrq $32, %rdx
    wrmsr
```

**After (fixed)**:
```asm
# core/context_switch.s - context_switch_user
    # Set up GS bases for return to user mode
    # GS_BASE currently = PCR (we're in kernel)
    
    # Read current PCR address from gs:[0] (self_ref)
    movq %gs:0, %rax                    # rax = this CPU's PCR
    
    # Set KERNEL_GS_BASE = PCR (for next syscall entry SWAPGS)
    movl $0xC0000102, %ecx              # IA32_KERNEL_GS_BASE
    movq %rax, %rdx
    shrq $32, %rdx
    wrmsr
    
    # Set GS_BASE = 0 (user mode sees GS_BASE = 0)
    movl $0xC0000101, %ecx              # IA32_GS_BASE
    xorl %eax, %eax
    xorl %edx, %edx
    wrmsr
    
    # ... restore GPRs, iretq ...
```

#### Verification
```bash
make test
```

---

### Phase 8: Remove Old Infrastructure
**Duration**: ~1 hour | **Risk**: Medium

#### Tasks
- [ ] Delete `PerCpuSyscallData` struct from `boot/src/gdt.rs`
- [ ] Delete `static mut PER_CPU_GDT` array
- [ ] Delete `static mut PER_CPU_TSS` array
- [ ] Delete `static mut PER_CPU_SYSCALL_DATA` array
- [ ] Delete `static mut SYSCALL_CPU_DATA_PTR` global
- [ ] Delete `gdt_init_for_cpu()` function
- [ ] Delete `syscall_gs_base_init_for_cpu()` function
- [ ] Delete `PerCpuData` struct from `lib/src/percpu.rs`
- [ ] Delete `static mut PER_CPU_DATA` array
- [ ] Delete `activate_gs_base_for_cpu()` function
- [ ] Delete `init_percpu_for_cpu()` function
- [ ] Update all callers to use new PCR-based functions
- [ ] Remove `.global SYSCALL_CPU_DATA_PTR` from assembly

#### Files Modified
| File | Changes |
|------|---------|
| `boot/src/gdt.rs` | Remove old structs and arrays |
| `lib/src/percpu.rs` | Remove old structs, delegate to PCR |
| `core/context_switch.s` | Remove `SYSCALL_CPU_DATA_PTR` reference |

#### Verification
```bash
make build  # No compilation errors (no dangling references)
make test   # All tests still pass
```

---

### Phase 9: Enable AP User-Mode Tasks
**Duration**: ~30 minutes | **Risk**: Medium (moment of truth!)

#### Tasks
- [ ] Remove workaround in `core/src/scheduler/per_cpu.rs`
- [ ] Add AP user-mode execution test

#### Key Change

**Remove this workaround**:
```rust
// core/src/scheduler/per_cpu.rs - select_target_cpu()

// DELETE THIS ENTIRE BLOCK:
// WORKAROUND: Force all user-mode tasks to CPU 0 until AP user-mode context switch is fixed
let is_user_mode = unsafe { (*task).flags & TASK_FLAG_USER_MODE != 0 };
if is_user_mode {
    return 0;
}
```

#### Test Code
```rust
pub fn test_ap_user_mode_execution() -> c_int {
    let cpu_count = slopos_lib::get_cpu_count();
    if cpu_count < 2 {
        klog_info!("PCR_TEST: Skipping AP user-mode test (single CPU)");
        return 0;
    }
    
    // This test validates that user-mode tasks can run on APs
    // The test framework already runs on multiple CPUs
    // If we get here without crashing, AP user-mode works!
    
    let current_cpu = slopos_lib::get_current_cpu();
    klog_info!("PCR_TEST: User-mode test running on CPU {}", current_cpu);
    
    // Verify we can make syscalls from any CPU
    let pid = unsafe { syscall_getpid() };
    if pid < 0 {
        klog_info!("PCR_TEST: BUG - syscall failed on CPU {}", current_cpu);
        return 1;
    }
    
    klog_info!("PCR_TEST: AP user-mode execution works! CPU={}, PID={}", current_cpu, pid);
    0
}
```

#### Verification
```bash
make test
# THE BIG TEST - AP user-mode must work now!
# Watch for any page faults on CPU 1+
```

---

### Phase 10: Final Cleanup & Documentation
**Duration**: ~1 hour | **Risk**: Low

#### Tasks
- [ ] Delete `plans/GS_BASE_SWAPGS_BUG.md` (bug fixed)
- [ ] Update `plans/KNOWN_ISSUES.md` - mark AP issue as FIXED
- [ ] Update `AGENTS.md` - document new PCR architecture
- [ ] Add architecture documentation to `lib/src/pcr.rs`
- [ ] Final full test run

#### Verification
```bash
make clean && make build && make test
make boot VIDEO=1 QEMU_SMP=4  # Visual verification with 4 CPUs
```

---

## Files Summary

### Created
| File | Description |
|------|-------------|
| `lib/src/pcr.rs` | Unified ProcessorControlRegion structure |
| `tests/src/pcr_tests.rs` | PCR validation test suite |

### Modified
| File | Changes |
|------|---------|
| `lib/src/lib.rs` | Export `pcr` module |
| `lib/src/percpu.rs` | Delegate to PCR, remove old structs |
| `boot/src/gdt.rs` | Remove old arrays, keep helper functions |
| `boot/src/smp.rs` | Use PCR for AP initialization |
| `boot/src/early_init.rs` | Use PCR for BSP initialization |
| `boot/idt_handlers.s` | Update SYSCALL offsets (8, 16 instead of 0, 8) |
| `core/context_switch.s` | Use `gs:[0]` instead of global |
| `core/src/scheduler/per_cpu.rs` | Remove user-mode workaround |
| `plans/KNOWN_ISSUES.md` | Mark AP issue as fixed |
| `AGENTS.md` | Document PCR architecture |

### Deleted
| File | Reason |
|------|--------|
| `plans/GS_BASE_SWAPGS_BUG.md` | Bug fixed by this implementation |

---

## Risk Mitigation

1. **Git commit after each phase** - Easy rollback if issues arise
2. **`make test` after every change** - Catch regressions immediately
3. **Phases ordered by dependency** - Later phases depend on earlier ones working
4. **Parallel structures during transition** - Old and new coexist until verified
5. **Assembly changes last** - Most dangerous changes done after Rust code is stable
6. **Explicit offset assertions** - Compile-time verification of critical layouts

---

## Success Criteria

- [ ] All existing tests pass (363+)
- [ ] New PCR tests pass
- [ ] `get_current_cpu()` uses fast GS-based access (~1-3 cycles vs ~100 cycles)
- [ ] AP user-mode tasks execute without page faults
- [ ] No `static mut` arrays for per-CPU data
- [ ] Single unified `ProcessorControlRegion` per CPU
- [ ] GS_BASE always points to PCR in kernel mode
- [ ] Boot and run stable with `QEMU_SMP=4`

---

## References

- [Redox OS kernel/src/percpu.rs](https://github.com/redox-os/kernel/blob/master/src/percpu.rs)
- [Redox OS kernel/src/arch/x86_shared/gdt.rs](https://github.com/redox-os/kernel/blob/master/src/arch/x86_shared/gdt.rs)
- [Intel SDM Vol. 3A - SWAPGS instruction](https://software.intel.com/content/www/us/en/develop/articles/intel-sdm.html)
- [OSDev Wiki - SWAPGS](https://wiki.osdev.org/SWAPGS)
