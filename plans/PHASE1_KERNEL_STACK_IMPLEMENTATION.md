# Phase 1: KernelStack Implementation Plan

## Context

**The problem being fixed**: SlopOS currently allocates task stacks via
`kmalloc` from the kernel heap. The kernel heap grows by pulling pages from
the page allocator, whose free range is `(total RAM) - (kernel image region)`.
Any kernel code growth shrinks max task capacity because `_kernel_end` moves
and the reserved kernel region rounds up to a 4KB page boundary.

**What Phase 1 delivers**: a `KernelStack` type that allocates its backing
from a dedicated kernel virtual address region, completely decoupled from
the kernel image. Growing kernel code no longer affects stack capacity.
Uses existing page-allocator primitives. Zero new `unsafe` surface in the
public API.

**Scope**: Phase 1 only — one VA region, one global allocator, no per-CPU
caching yet. That's Phase 2.

## Research Summary

### Primitives available (verified in code)

| Primitive | Location | Signature |
|---|---|---|
| Alloc single frame | `mm/src/page_alloc.rs:870` | `alloc_page_frame(flags: u32) -> PhysAddr` |
| Free single frame | `mm/src/page_alloc.rs:874` | `free_page_frame(phys: PhysAddr) -> c_int` |
| Map kernel 4KB page | `mm/src/paging/tables.rs:399` | `map_page_4kb(va: VirtAddr, pa: PhysAddr, flags: u64) -> c_int` |
| Unmap kernel page | `mm/src/paging/tables.rs:527` | `unmap_page(va: VirtAddr) -> PhysAddr` |
| Type-safe VA | `abi/src/addr.rs` | `VirtAddr::new(u64)`, canonical-form enforced |
| Type-safe PA | `abi/src/addr.rs` | `PhysAddr::new(u64)` |
| Page flags | `mm/src/paging_defs.rs` | `PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE` |

### Reference pattern already in tree

`boot/src/ist_stacks.rs` does exactly the pattern we need — but only for
exception stacks (IST), statically one-per-CPU:
- Reserves a fixed VA region: `EXCEPTION_STACK_REGION_BASE = 0xFFFF_FFFF_C000_0000`
- Per-index virtual range via stride arithmetic
- On-demand physical frame allocation + `map_page_4kb`
- Guard page left unmapped below each stack for overflow detection

`KernelStack` generalizes this pattern for dynamic task stacks.

### Current task-stack callsites (what we migrate)

| Location | What it does |
|---|---|
| `core/src/scheduler/task/task_lifecycle.rs:142` | `kmalloc(stack_size)` inside `KernelStackLease::allocate` |
| `core/src/scheduler/task/task_lifecycle.rs:165` | `kfree(base)` inside `Drop for KernelStackLease` |
| `core/src/scheduler/task/task_lifecycle.rs:220` | `kfree(kernel_stack_base)` in `cleanup_task_create_resources` |
| `core/src/scheduler/task/task_table.rs:71,76` | `kfree` for kernel-mode task stacks in `free_task_stacks` |

Call volume: 2 allocation sites (kernel task / user task RSP0), 3 free sites.
All inside the scheduler module.

### Address space layout (free room verified)

| Region | Start | End |
|---|---|---|
| Kernel heap | `0xFFFF_FFFF_9000_0000` | `0xFFFF_FFFF_A000_0000` (256 MB) |
| **KSTACK region (new)** | `0xFFFF_FFFF_A000_0000` | `0xFFFF_FFFF_C000_0000` (**512 MB**) |
| Exception stacks (IST) | `0xFFFF_FFFF_C000_0000` | higher |

512 MB / 64 KB stride = **8192 stack slots**. Phase 1 doesn't need all of it;
raising MAX_TASKS to 256 uses 16 MB.

## Target Design

### Public API (safe)

```rust
// core/src/scheduler/stack.rs

pub struct KernelStack {
    slot: KstackSlot,     // Owning handle into the VA allocator (RAII).
    size: usize,          // Usable stack size (excludes guard).
}

#[derive(Debug)]
pub enum StackAllocError {
    OutOfVirtualSpace,    // No free slot in KSTACK region.
    OutOfPhysicalFrames,  // Page allocator returned null.
    MappingFailed,        // map_page_4kb error (e.g., no frames for page tables).
    InvalidSize,          // Size not a multiple of 4KB, or too large.
}

impl KernelStack {
    /// Allocate a kernel stack of `size` bytes (must be 4KB-multiple).
    /// Usable range: [base(), top()).  One unmapped guard page below base().
    pub fn allocate(size: usize) -> Result<Self, StackAllocError>;

    /// Highest address (exclusive, stack grows downward from here).
    pub fn top(&self) -> VirtAddr;

    /// Lowest usable address (inclusive).
    pub fn base(&self) -> VirtAddr;

    /// Size in bytes (excludes guard page).
    pub fn size(&self) -> usize;
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Unmap PTEs, return physical frames, release VA slot.
    }
}
```

### Internal layers

```
┌───────────────────────────────────────────────┐
│ KernelStack (RAII handle, safe API)           │  core/src/scheduler/stack.rs
├───────────────────────────────────────────────┤
│ KstackSlot (owns a VA range slot)             │  mm/src/kstack_va.rs
│ KstackVaAllocator (bitmap over 512 MB region) │
├───────────────────────────────────────────────┤
│ Existing primitives:                          │
│   alloc_page_frame / free_page_frame          │  mm/src/page_alloc.rs
│   map_page_4kb / unmap_page                   │  mm/src/paging/tables.rs
└───────────────────────────────────────────────┘
```

### Memory layout per slot

```
Slot N (64 KB stride)
 ┌──────────────────────────┐ ← slot_base + 0x10000  = top() [stack grows down]
 │  mapped stack pages      │
 │  (size bytes, 4KB units) │
 ├──────────────────────────┤ ← slot_base + 0x1000   = base()
 │  unmapped guard page     │
 └──────────────────────────┘ ← slot_base
```

- Default stack size: 32 KB (= `TASK_KERNEL_STACK_SIZE`, unchanged).
- Guard page: 1 × 4 KB unmapped at the bottom. Overflow → page fault.
- Slot stride: 64 KB (headroom for future 40 KB+ stacks without layout change).

### Why this is safe without per-stack unsafe

- `KernelStack` owns `KstackSlot` which owns the VA range (RAII).
- Drop order: unmap PTEs → free physical frames → release VA slot.
- One `unsafe` call inside `KernelStack::allocate` for the C-style
  `map_page_4kb` FFI; checked for errors at each step. Wrapped in a safe
  function.
- The VA allocator's free-list / bitmap is plain safe Rust (`IrqMutex`-protected).
- Physical frame allocation is already safe.

## Files

### New
- `mm/src/kstack_va.rs` — `KstackSlot`, `KstackVaAllocator`, `KSTACK_VA_ALLOCATOR` static, `alloc_slot` / `release_slot` module functions. ~150 LOC.
- `core/src/scheduler/stack.rs` — `KernelStack`, `StackAllocError`, `Drop` impl. ~150 LOC.

### Modified
- `mm/src/memory_layout_defs.rs` — add `KSTACK_VA_BASE`, `KSTACK_VA_END`, `KSTACK_STRIDE`, `KSTACK_GUARD_SIZE`, `KSTACK_MAX_SLOTS` constants.
- `mm/src/lib.rs` — re-export `kstack_va` module.
- `mm/src/memory_init.rs` — call `kstack_va::init()` after `init_paging()` and `init_kernel_heap()`. Reserve the KSTACK VA region so nothing else uses it.
- `core/src/scheduler/mod.rs` — declare `stack` submodule.
- `core/src/scheduler/task/task_struct.rs` — **no layout change**; existing `kernel_stack_base/top/size` u64 fields remain, populated from `KernelStack::base()`/`top()`/`size()`. The owning `KernelStack` handle stored in a new `Option<KernelStack>` field so Drop runs on task free.
- `core/src/scheduler/task/task_lifecycle.rs` — replace `KernelStackLease` usage with `KernelStack::allocate()`. Store handle in task. `disarm` pattern disappears — the handle itself is the ownership token.
- `core/src/scheduler/task/task_table.rs` — remove explicit `kfree` in `free_task_stacks` for kernel-mode stacks; replaced by `task.kernel_stack.take()` → Drop.
- `abi/src/task.rs` — raise `MAX_TASKS` from 64 to 256 (soft bump; Phase 3 removes the hard bound entirely).
- `core/src/scheduler/sched_tests.rs` — `test_create_max_tasks` reads `MAX_TASKS`, no change beyond that. Add a new test `test_task_capacity_independent_of_kernel_image_size` (see Verification).

## Step-by-step Plan

### Step 1 — Memory layout constants (~10 min)

In `mm/src/memory_layout_defs.rs`:

```rust
/// Base of the kernel-stack virtual region.
pub const KSTACK_VA_BASE: u64 = 0xFFFF_FFFF_A000_0000;
/// End (exclusive).  512 MB region = 8192 slots of 64 KB stride.
pub const KSTACK_VA_END: u64 = 0xFFFF_FFFF_C000_0000;
/// Stride per slot: 1 guard page + up to 60 KB usable.  Rounded to 64 KB.
pub const KSTACK_STRIDE: u64 = 0x10000;
/// Guard page size (unmapped, one 4 KB page per slot).
pub const KSTACK_GUARD_SIZE: u64 = 0x1000;
/// Max slots (derived): (KSTACK_VA_END - KSTACK_VA_BASE) / KSTACK_STRIDE.
pub const KSTACK_MAX_SLOTS: usize = 8192;
```

### Step 2 — VA slot allocator (~1–2 hours)

New file `mm/src/kstack_va.rs`:

```rust
use core::sync::atomic::AtomicU32;
use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR};
use slopos_abi::addr::VirtAddr;
use crate::memory_layout_defs::*;

/// Bitmap of free slots.  1 = free.  8192 slots = 128 u64 words.
struct KstackVaAllocator {
    // 8192 bits in 128 u64 words.  Initialised all-free in init().
    free_bitmap: [u64; 128],
    hint: u32,      // Rotating search hint for alloc.
    in_use: u32,    // Debug/stats.
}

static KSTACK_VA_ALLOCATOR: IrqMutex<KstackVaAllocator> =
    IrqMutex::new(KstackVaAllocator::new(), LOCK_LEVEL_ALLOCATOR);

/// Opaque handle to an allocated slot.  Automatically returned on drop.
pub struct KstackSlot {
    idx: u32,  // 0..KSTACK_MAX_SLOTS
}

impl KstackSlot {
    /// Base virtual address of this slot's stride (lowest address).
    pub fn va_base(&self) -> VirtAddr {
        VirtAddr::new(KSTACK_VA_BASE + self.idx as u64 * KSTACK_STRIDE)
    }
}

impl Drop for KstackSlot {
    fn drop(&mut self) {
        KSTACK_VA_ALLOCATOR.lock().release(self.idx);
    }
}

pub fn alloc_slot() -> Option<KstackSlot> {
    KSTACK_VA_ALLOCATOR.lock().alloc()
}

/// Call once during memory init, after paging is verified.
pub(crate) fn init() { /* all-free bitmap */ }
```

Implementation notes:
- `alloc()`: scan from `hint`, find first zero bit, clear it, bump hint.
- `release(idx)`: set bit.
- Test module with `#[cfg(test)]` or itests to verify alloc/release correctness.

### Step 3 — KernelStack handle (~2–3 hours)

New file `core/src/scheduler/stack.rs`:

```rust
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_mm::kstack_va::{alloc_slot, KstackSlot};
use slopos_mm::page_alloc::{alloc_page_frame, free_page_frame};
use slopos_mm::paging::{map_page_4kb, unmap_page};
use slopos_mm::paging_defs::{PageFlags, PAGE_SIZE_4KB};
use slopos_mm::memory_layout_defs::{KSTACK_GUARD_SIZE, KSTACK_STRIDE};

#[derive(Debug)]
pub enum StackAllocError {
    OutOfVirtualSpace,
    OutOfPhysicalFrames,
    MappingFailed,
    InvalidSize,
}

pub struct KernelStack {
    slot: KstackSlot,
    size: usize,  // bytes mapped; excludes guard
}

impl KernelStack {
    pub fn allocate(size: usize) -> Result<Self, StackAllocError> {
        // 1) Validate
        if size == 0 || size % PAGE_SIZE_4KB != 0 {
            return Err(StackAllocError::InvalidSize);
        }
        let usable = size as u64;
        if usable + KSTACK_GUARD_SIZE > KSTACK_STRIDE {
            return Err(StackAllocError::InvalidSize);
        }

        // 2) Reserve VA slot (RAII)
        let slot = alloc_slot().ok_or(StackAllocError::OutOfVirtualSpace)?;

        // 3) Allocate physical frames + map each mapped page
        //    Layout: [guard unmapped][usable mapped pages]
        let first_mapped_va = slot.va_base().as_u64() + KSTACK_GUARD_SIZE;
        let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();
        let n_pages = size / PAGE_SIZE_4KB;

        let mut mapped = 0;
        for i in 0..n_pages {
            let pa = alloc_page_frame(0);
            if pa.is_null() {
                // Cleanup partial mapping
                Self::cleanup_partial(&slot, mapped);
                return Err(StackAllocError::OutOfPhysicalFrames);
            }
            let va = VirtAddr::new(first_mapped_va + (i * PAGE_SIZE_4KB) as u64);
            if unsafe { map_page_4kb(va, pa, flags) } != 0 {
                free_page_frame(pa);
                Self::cleanup_partial(&slot, mapped);
                return Err(StackAllocError::MappingFailed);
            }
            mapped += 1;
        }

        Ok(Self { slot, size })
    }

    pub fn base(&self) -> VirtAddr {
        VirtAddr::new(self.slot.va_base().as_u64() + KSTACK_GUARD_SIZE)
    }

    pub fn top(&self) -> VirtAddr {
        VirtAddr::new(self.base().as_u64() + self.size as u64)
    }

    pub fn size(&self) -> usize { self.size }

    fn cleanup_partial(slot: &KstackSlot, mapped_count: usize) {
        let first = slot.va_base().as_u64() + KSTACK_GUARD_SIZE;
        for i in 0..mapped_count {
            let va = VirtAddr::new(first + (i * PAGE_SIZE_4KB) as u64);
            let pa = unsafe { unmap_page(va) };
            if !pa.is_null() {
                free_page_frame(pa);
            }
        }
        // slot drops naturally when caller returns Err
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let first = self.slot.va_base().as_u64() + KSTACK_GUARD_SIZE;
        let n_pages = self.size / PAGE_SIZE_4KB;
        for i in 0..n_pages {
            let va = VirtAddr::new(first + (i * PAGE_SIZE_4KB) as u64);
            let pa = unsafe { unmap_page(va) };
            if !pa.is_null() {
                free_page_frame(pa);
            }
        }
        // self.slot drops → KstackSlot::drop → VA slot released
    }
}
```

**Unsafe audit**: two calls — `map_page_4kb` and `unmap_page`. Both are FFI
into the C-style API. Wrapping them in a safe function is fine because:
- VA is valid (obtained from the slot allocator, canonical-form enforced by `VirtAddr`).
- PA is valid (from `alloc_page_frame` or a PTE the walker returns).
- Caller exclusivity: the slot is owned by us until Drop.

### Step 4 — Wire into memory init (~30 min)

In `mm/src/memory_init.rs::init_memory_system()`, after `init_paging()` and
`init_kernel_heap()`:

```rust
crate::kstack_va::init();
klog_debug!("Kernel stack VA allocator ready: {} slots, {} MB region",
    KSTACK_MAX_SLOTS, (KSTACK_VA_END - KSTACK_VA_BASE) / (1024 * 1024));
```

Also add a reservation for the KSTACK VA region so the rest of the kernel
knows it's owned by this allocator (defensive; nothing else uses it today).

### Step 5 — Migrate task_create (~1 hour)

In `core/src/scheduler/task/task_lifecycle.rs`:

Replace the `KernelStackLease` struct entirely. New `TaskCreateResources`
carries `kernel_stack: KernelStack` (owning). `allocate_kernel_task_resources`:

```rust
fn allocate_kernel_task_resources() -> Option<TaskCreateResources> {
    let stack = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("task_create: kernel stack allocation failed: {:?}", e);
            return None;
        }
    };
    Some(TaskCreateResources {
        process_id: INVALID_PROCESS_ID,
        stack_base: stack.base().as_u64(),
        kernel_stack_base: stack.base().as_u64(),
        kernel_stack_size: TASK_STACK_SIZE,
        kernel_stack: Some(stack),   // new field, owned
    })
}
```

Analogous for `allocate_user_task_resources`.

On success, the `KernelStack` moves into the `Task` struct (new field
`kernel_stack: Option<KernelStack>`). On failure, the local drops →
automatic cleanup. **`disarm()` pattern goes away entirely.**

### Step 6 — Migrate task_destroy path (~30 min)

In `core/src/scheduler/task/task_table.rs::free_task_stacks`:

```rust
pub(super) fn free_task_stacks(task: *mut Task) {
    if task.is_null() { return; }
    unsafe {
        // KernelStack drop unmaps + frees + releases slot.
        (*task).kernel_stack = None;
        (*task).kernel_stack_base = 0;
        (*task).kernel_stack_top = 0;

        // User-mode user-space stack (in process VM) still freed elsewhere.
    }
}
```

Remove `kfree` calls. The explicit `kfree` in `cleanup_task_create_resources`
also disappears — local `KernelStack` drops.

### Step 7 — Raise MAX_TASKS to 256 (~5 min)

In `abi/src/task.rs`: `pub const MAX_TASKS: usize = 256;`

Verify the static `[Task; MAX_TASKS]` array in `task_table.rs` still fits
in BSS (each Task is a few KB → 256 tasks × ~4 KB = ~1 MB; fine).

### Step 8 — Add regression-proof test (~30 min)

In `core/src/scheduler/sched_tests.rs`, add:

```rust
pub fn test_kstack_capacity_independent_of_image_size() -> TestResult {
    // Build a stack, drop it, build again.  Verify freed slot is reused.
    let s1 = KernelStack::allocate(TASK_STACK_SIZE as usize).unwrap();
    let top1 = s1.top();
    drop(s1);
    let s2 = KernelStack::allocate(TASK_STACK_SIZE as usize).unwrap();
    assert_eq!(s2.top(), top1, "slot must be reused after drop");
    TestResult::Pass
}
```

And document in a comment: "adding N KB of kernel code changes `_kernel_end`
but does not affect `KSTACK_VA_BASE`, so stack capacity is unchanged."

### Step 9 — Format, build, test, commit (~20 min)

```
cargo fmt --all
just build
just test
```

Expected: SUITE61 passes regardless of the TCP binary bloat.
Create new tests pass, existing tests unchanged.

## Verification

### Build
- `cargo fmt --all` (mandatory pre-commit)
- `just build` — no warnings from new code.

### Run
- `just test` — full QEMU harness.
  - **SUITE61 must pass** (regression is fixed by design).
  - All existing suites unchanged.
- `just boot-log` — serial log sanity.

### Regression proof
Add ~100 KB of dummy kernel code (`#[used] static FAT: [u8; 102400] = [0; 102400];`
in some module) and re-run `just test`. **Task creation capacity must be
identical.** This proves the decoupling.

### Safety audit
- Count `unsafe` in public API: should be zero.
- Count `unsafe` in new code total: ≤ 4 (map_page_4kb × 2 sites, unmap_page × 2 sites, all inside `KernelStack::allocate` / `Drop` / cleanup).
- Verify `KernelStack` isn't `Copy` or `Clone` (prevents double-free).
- Verify `Drop` runs on both success-path task destroy AND failure-path
  cleanup (moving into Task → Option::take drops; leaving scope in
  `allocate` error path drops the partial handle).

## Commit plan

Single commit (Phase 1 is cohesive):

```
scheduler+mm: decouple task stacks from kernel image (Phase 1)

Task kernel stacks are now backed by a dedicated virtual address
region (0xFFFF_FFFF_A0000000..0xFFFF_FFFF_C0000000, 512 MB, 8192 slots),
with physical frames allocated on demand from the page allocator and
mapped into the region.  Each stack has an unmapped guard page below
it for overflow detection.

Removes the coupling where kernel-image size reduced max task count.
Fixes the SUITE61 regression caused by Phase 6 TCP code growth.

Introduces a typed `KernelStack` RAII handle (core/scheduler/stack.rs)
over a `KstackSlot` handle from the VA allocator (mm/kstack_va.rs).
Zero unsafe in the public API; two `map_page_4kb` / `unmap_page` FFI
calls confined to the implementation.

Raises MAX_TASKS from 64 to 256.  Phase 3 removes the hard bound.
```

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| `map_page_4kb` failure mid-allocation leaves partial mapping | `cleanup_partial` unmaps + frees what was mapped before returning Err |
| Slot bitmap corruption | `IrqMutex<KstackVaAllocator>`, plain safe Rust; covered by unit test |
| Stack overflow silently corrupts neighbour | Guard page unmapped → page fault → clean panic |
| Migrating Task struct breaks context switch | Keep existing u64 fields populated from handle; only add new `Option<KernelStack>` field |
| User tasks accidentally freed via KernelStack | Only kernel RSP0 stacks use `KernelStack`; user-space stacks remain in process VM |

## Out of scope (defer to later phases)

- Per-CPU frame caching (**Phase 2**) — current code calls into global allocator every page; fine for Phase 1, improves later.
- Removing MAX_TASKS hard bound (**Phase 3**) — requires swapping `[Task; MAX_TASKS]` for dynamic storage.
- SafeStack / dual-stack hardening (**Phase 4**) — compiler integration required.
- Porting user-space stacks to the same mechanism — user stacks live in process VM; different problem.
