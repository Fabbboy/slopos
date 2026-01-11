# SlopOS Architecture Restructure: Introducing the `core` Crate

## Executive Summary

This plan proposes restructuring SlopOS's crate dependencies by introducing a new `core` crate that owns kernel policy (scheduler, syscalls, IRQ framework), while `drivers` becomes a pure hardware abstraction layer. This eliminates the current `sched_bridge` runtime indirection pattern and creates a clean, one-directional dependency graph.

**End State**: No legacy code remains. The `sched` crate is deleted, `sched_bridge.rs` is deleted, all `SchedulerServices`/`BootServices` traits are removed from `abi`, and all call sites use the new `core` APIs directly.

---

## Current Implementation Status

> **Last Updated**: January 2025

### Phases Completed

| Phase | Status | Notes |
|-------|--------|-------|
| Phase 1: Create `core` crate and move scheduler | ✅ COMPLETE | Core crate exists with scheduler, platform services, IRQ framework |
| Phase 3: Implement platform services and remove bridge | ✅ COMPLETE | `sched_bridge.rs` deleted, platform services working |
| Phase 4: Move IRQ framework to core | ✅ COMPLETE | `core/src/irq.rs` has dispatch logic |
| Phase 5: Delete `sched` crate and legacy traits | ✅ COMPLETE | `sched/` deleted, traits removed |
| **Phase 2: Move syscalls to core** | ❌ NOT DONE | See "Remaining Work" section |

### What Works Now

- ✅ `core` crate exists with scheduler, IRQ framework, platform services, wl_currency
- ✅ `sched/` crate deleted
- ✅ `drivers/src/sched_bridge.rs` deleted
- ✅ `SchedulerServices` and `BootServices` traits removed
- ✅ Dependency graph is correct: `core` has NO dependency on `drivers`
- ✅ `drivers` depends on `core`
- ✅ Build passes, tests pass, system boots
- ✅ `abi/src/sched_traits.rs` renamed to `abi/src/fate.rs` (now only contains `FateResult`)
- ✅ `wl_currency` consolidated in `core` (drivers re-exports for compatibility)

### Verification Results (All Pass)

```bash
grep -r "sched_bridge" --include="*.rs" .     # ✅ Nothing
grep -r "SchedulerServices" --include="*.rs" . # ✅ Nothing
grep -r "BootServices" --include="*.rs" .      # ✅ Nothing
grep -r "slopos-sched" .                       # ✅ Nothing
grep -r "slopos_sched" --include="*.rs" .      # ✅ Nothing
ls sched/                                       # ✅ No such directory
make build                                      # ✅ Succeeds
make test                                       # ✅ Passes
```

---

## Remaining Work: Phase 2 - Move Syscalls to Core

### Current State

Syscalls remain in `drivers/src/`:
- `syscall.rs` - dispatch entry point
- `syscall_handlers.rs` - all handler implementations
- `syscall_common.rs` - utilities
- `syscall_context.rs` - context extraction
- `syscall_fs.rs` - filesystem syscalls
- `syscall_macros.rs` - `define_syscall!` macro
- `syscall_types.rs` - type re-exports

### Why It Wasn't Done

Moving syscalls to `core` requires **abstracting all driver services** used by syscall handlers. The handlers currently call directly into:

| Driver Module | Functions Used |
|---------------|----------------|
| `input_event` | `input_poll`, `input_set_keyboard_focus`, `input_set_pointer_focus`, etc. |
| `random` | `random_next()` |
| `serial` | `write_str()` |
| `video_bridge` | 20+ functions: `surface_commit`, `roulette_draw`, `framebuffer_get_info`, etc. |
| `fate` | `fate_notify_outcome()` |
| `irq` | `get_timer_ticks()` |
| `pit` | `pit_get_frequency()`, `pit_poll_delay_ms()`, `pit_sleep_ms()` |
| `tty` | `tty_read_line()`, `tty_read_char_blocking()`, `tty_set_focus()`, etc. |

If syscalls move to `core`, and `core` cannot depend on `drivers`, then all these services must be abstracted via callback registration.

### Required Changes to Complete Phase 2

#### 1. Extend Platform Services (or create Syscall Services)

Add callback registrations for each driver service used by syscalls:

```rust
// core/src/syscall_services.rs (NEW)

pub struct SyscallServices {
    // Input services
    pub input_poll: fn(u32) -> Option<InputEvent>,
    pub input_set_keyboard_focus: fn(u32),
    pub input_set_pointer_focus: fn(u32, u64),
    pub input_event_count: fn(u32) -> usize,
    // ... 10+ more input functions
    
    // Video services
    pub surface_commit: fn(u32) -> i32,
    pub surface_request_frame: fn(u32) -> i32,
    pub framebuffer_get_info: fn() -> *mut FramebufferInfoC,
    pub roulette_draw: fn(u32) -> Result<(), VideoError>,
    // ... 20+ more video functions
    
    // TTY services
    pub tty_read_line: fn(*mut u8, usize) -> usize,
    pub tty_read_char_blocking: fn(*mut u8) -> i32,
    pub tty_set_focus: fn(u32) -> i32,
    // ... more tty functions
    
    // Fate services
    pub fate_notify_outcome: fn(*const FateResult),
}

static SYSCALL_SERVICES: AtomicPtr<SyscallServices> = AtomicPtr::new(ptr::null_mut());

pub fn register_syscall_services(services: &'static SyscallServices) { ... }
pub fn syscall_services() -> &'static SyscallServices { ... }
```

#### 2. Register Services from Drivers

```rust
// drivers/src/syscall_services_init.rs (NEW)

use slopos_core::syscall_services::{SyscallServices, register_syscall_services};

static SERVICES: SyscallServices = SyscallServices {
    input_poll: |task_id| crate::input_event::input_poll(task_id),
    input_set_keyboard_focus: |task_id| crate::input_event::input_set_keyboard_focus(task_id),
    surface_commit: |task_id| crate::video_bridge::surface_commit(task_id),
    tty_read_line: |buf, len| crate::tty::tty_read_line(buf, len),
    // ... all other functions
};

pub fn init() {
    register_syscall_services(&SERVICES);
}
```

#### 3. Update Boot Sequence

```rust
// boot/src/early_init.rs

// After drivers init, before syscalls can be called:
slopos_drivers::syscall_services_init::init();
```

#### 4. Move and Refactor Syscall Files

Move files to `core/src/syscall/`:
```
core/src/syscall/
├── mod.rs
├── dispatch.rs      (from drivers/src/syscall.rs)
├── handlers.rs      (from drivers/src/syscall_handlers.rs)
├── common.rs        (from drivers/src/syscall_common.rs)
├── context.rs       (from drivers/src/syscall_context.rs)
├── fs.rs            (from drivers/src/syscall_fs.rs)
├── macros.rs        (from drivers/src/syscall_macros.rs)
└── types.rs         (from drivers/src/syscall_types.rs)
```

#### 5. Refactor All Handler Functions

Change every driver call to go through the service abstraction:

```rust
// BEFORE (in drivers):
use crate::video_bridge;
let rc = video_bridge::surface_commit(task_id);

// AFTER (in core):
use crate::syscall_services::syscall_services;
let rc = (syscall_services().surface_commit)(task_id);
```

This affects 50+ call sites across all syscall handlers.

#### 6. Update IDT Entry Point

```rust
// boot/src/idt.rs
// BEFORE:
use slopos_drivers::syscall::syscall_handle;

// AFTER:
use slopos_core::syscall::syscall_handle;
```

#### 7. Remove Syscall Modules from Drivers

```rust
// drivers/src/lib.rs
// Remove:
pub mod syscall;
pub mod syscall_handlers;
pub mod syscall_common;
pub mod syscall_context;
pub mod syscall_fs;
pub mod syscall_macros;
pub mod syscall_types;
```

#### 8. Delete Old Files

```bash
rm drivers/src/syscall.rs
rm drivers/src/syscall_handlers.rs
rm drivers/src/syscall_common.rs
rm drivers/src/syscall_context.rs
rm drivers/src/syscall_fs.rs
rm drivers/src/syscall_macros.rs
rm drivers/src/syscall_types.rs
```

### Effort Estimate

| Task | Files Changed | Complexity |
|------|---------------|------------|
| Create SyscallServices struct | 1 new file | Medium |
| Register services from drivers | 1 new file | Medium |
| Move syscall files to core | 7 files | Low |
| Refactor all handler functions | 7 files, 50+ call sites | High |
| Update boot sequence | 2 files | Low |
| Update IDT entry | 1 file | Low |
| Delete old files | 7 files | Low |
| Testing and debugging | - | Medium-High |

**Total estimated effort**: 2-4 hours of focused work

### Alternative: Keep Syscalls in Drivers

The current architecture works correctly. Keeping syscalls in drivers is a valid choice that:
- ✅ Avoids 50+ function pointer indirections
- ✅ Keeps syscall handlers simple (direct calls)
- ✅ Still maintains correct dependency graph (core has no dependency on drivers)
- ❌ Doesn't match the plan's "core owns all policy" vision
- ❌ Syscalls are split between drivers (handlers) and core (scheduler calls)

---

## Problem Statement

### Current Architecture

```
abi (leaf - no deps, contains traits like SchedulerServices, BootServices)
 ^
lib -> abi (CPU primitives, ports, TSC, spinlocks)
 ^
mm -> lib, abi (memory management, paging, heap)
 ^
drivers -> mm, lib, abi, fs (PIT, APIC, IOAPIC, IRQ, serial, keyboard, PCI, syscalls)
 ^
sched -> drivers, mm, lib, abi, fs (scheduler, tasks, context switch)
 ^
boot -> sched, drivers, video, mm, lib, abi
 ^
kernel -> boot, sched, drivers, video, mm, lib, userland
```

### The Circular Dependency Problem

`sched` depends on `drivers`, but `drivers` needs scheduler functionality:

**What `sched` needs from `drivers`:**
- `pit::pit_get_frequency()`, `pit::pit_poll_delay_ms()` - timing
- `random::random_next()` - RNG for fate/roulette
- `wl_currency::award_win/loss()` - W/L currency system
- `sched_bridge::gdt_set_kernel_rsp0()` - TSS RSP0 update on context switch
- `sched_bridge::video_task_cleanup()` - cleanup hooks
- `serial::serial_putc_com1()` - debug output

**What `drivers` needs from `sched`:**
- `timer_tick()` - called from IRQ handler
- `schedule()`, `yield_cpu()` - called from syscall handlers
- `get_current_task()` - needed for blocking I/O, TTY
- `block_current_task()`, `unblock_task()` - blocking primitives
- `task_terminate()` - from syscall
- `kernel_panic()`, `kernel_shutdown()`, `kernel_reboot()` - fatal paths
- `request_reschedule_from_interrupt()` - preemption

### Current "Solution" - The Bridge Pattern

Traits defined in `abi/src/sched_traits.rs`:
```rust
pub trait SchedulerServices: Send + Sync {
    fn timer_tick(&self);
    fn schedule(&self);
    fn get_current_task(&self) -> TaskRef;
    // ... 20+ methods
}

pub trait BootServices: Send + Sync {
    fn kernel_panic(&self, msg: *const c_char) -> !;
    fn gdt_set_kernel_rsp0(&self, rsp0: u64);
    // ...
}
```

`drivers/src/sched_bridge.rs` holds `Once<&'static dyn SchedulerServices>`, and `sched` registers itself at runtime.

### Why the Bridge Pattern is Problematic

| Issue | Impact |
|-------|--------|
| **Runtime indirection** | Every scheduler call goes through `Option::map()` + vtable dispatch |
| **Initialization ordering** | If anything calls `sched_bridge::*` before `init_scheduler()`, it silently fails |
| **Hidden coupling** | The "bridge" obscures the real dependencies |
| **Defensive overhead** | 48 call sites do `if let Some(s) = sched() { ... }` checks that should never fail |
| **Not how production kernels work** | Linux doesn't do this - subsystems call each other directly |

---

## Proposed Architecture

### Target Dependency Graph

```
                    +---------------------------------------------+
                    |               kernel                         |
                    |  (main.rs - orchestration only)             |
                    +----------------------+----------------------+
                                           |
          +--------------------------------+--------------------------------+
          |                                |                                |
          v                                v                                v
    +----------+                    +----------+                    +----------+
    |  video   |                    |   boot   |                    | userland |
    +----+-----+                    +----+-----+                    +----+-----+
         |                               |                               |
         +---------------+---------------+---------------+---------------+
                         |                               |
                         v                               v
               +-----------------+               +-----------------+
               |      core       | <--- NEW      |       fs        |
               |                 |               |                 |
               | * scheduler     |               | * ramfs         |
               | * task model    |               | * vfs           |
               | * syscall dispatch              |                 |
               | * irq framework |               +-----------------+
               | * wait queues   |                       |
               | * panic/shutdown|                       |
               | * platform traits                       |
               +--------+--------+                       |
                        |                                |
          +-------------+-------------+                  |
          |             |             |                  |
          v             v             v                  |
    +----------+  +----------+  +----------+             |
    | drivers  |  |    mm    |  |   lib    | <-----------+
    |          |  |          |  |          |
    | * PIT    |  | * paging |  | * CPU    |
    | * APIC   |  | * heap   |  | * ports  |
    | * serial |  | * shm    |  | * spinlock
    | * kbd    |  |          |  |          |
    | * PCI    |  +----+-----+  +----+-----+
    +----+-----+       |             |
         |             +------+------+
         |                    |
         v                    v
    +----------+        +----------+
    |   lib    |        |   abi    |
    +----+-----+        +----------+
         |
         v
    +----------+
    |   abi    |
    +----------+
```

### Key Insight

**Core owns policy, drivers provide services.**

The fundamental restructuring is:
- `core` contains kernel policy decisions (scheduler, syscalls, IRQ dispatch)
- `drivers` contains hardware implementations and registers services with core
- The dependency flows ONE direction: `drivers` -> `core` (to register), NOT `core` -> `drivers`

---

## Detailed Design

### 1. Platform Services (Replacing the Bridge)

Instead of trait objects with vtable dispatch, use function pointer tables for hot paths:

```rust
// core/src/platform.rs

use core::sync::atomic::{AtomicPtr, Ordering};

/// Function pointer table for platform services (no vtables in hot paths)
#[repr(C)]
pub struct PlatformServices {
    // Timer services (from PIT driver)
    pub timer_get_ticks: fn() -> u64,
    pub timer_get_frequency: fn() -> u32,
    pub timer_poll_delay_ms: fn(u32),
    
    // Console services (from serial driver)
    pub console_putc: fn(u8),
    pub console_puts: fn(&[u8]),
    
    // RNG services (from random driver)
    pub rng_next: fn() -> u64,
    
    // GDT services (from boot/arch)
    pub gdt_set_kernel_rsp0: fn(u64),
}

static PLATFORM: AtomicPtr<PlatformServices> = AtomicPtr::new(core::ptr::null_mut());

/// Called once by drivers during init - panics if already registered
pub fn register_platform(services: &'static PlatformServices) {
    let prev = PLATFORM.swap(services as *const _ as *mut _, Ordering::Release);
    assert!(prev.is_null(), "platform already registered");
}

/// Fast path accessor - panics if not initialized (invariant, not defensive)
#[inline(always)]
pub fn platform() -> &'static PlatformServices {
    let ptr = PLATFORM.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "platform not initialized");
    unsafe { &*ptr }
}

// Convenience wrappers with inlining
#[inline(always)]
pub fn timer_ticks() -> u64 {
    (platform().timer_get_ticks)()
}

#[inline(always)]
pub fn timer_frequency() -> u32 {
    (platform().timer_get_frequency)()
}

#[inline(always)]
pub fn get_time_ms() -> u64 {
    let ticks = timer_ticks();
    let freq = timer_frequency();
    (ticks * 1000) / freq as u64
}
```

**Why function pointers over trait objects:**
- No vtable indirection (single pointer dereference vs vtable lookup)
- Explicit, predictable call overhead
- Easier to reason about in hot paths (timer tick, syscalls)

### 2. Event Signaling (Drivers -> Core)

Instead of drivers calling `sched_bridge::timer_tick()`, they signal events:

```rust
// core/src/events.rs

use crate::scheduler;
use crate::waitqueue;

/// Called by IRQ dispatch when timer fires
#[inline]
pub fn on_timer_interrupt() {
    scheduler::timer_tick();
}

/// Called by keyboard driver when input available  
pub fn on_input_available() {
    waitqueue::wake_one(&INPUT_WAITERS);
}

/// Called by drivers when a blocked operation completes
pub fn on_io_complete(task: TaskRef) {
    scheduler::unblock_task(task);
}
```

```rust
// drivers/src/irq.rs (simplified)

use slopos_core::events;

extern "C" fn timer_irq_handler(...) {
    TIMER_TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Signal event to core - don't call scheduler directly
    events::on_timer_interrupt();
}

extern "C" fn keyboard_irq_handler(...) {
    // Handle scancode...
    keyboard::keyboard_handle_scancode(scancode);
    // Signal that input is available
    events::on_input_available();
}
```

### 3. Core Crate Structure

**Current actual structure:**
```
core/
├── Cargo.toml
├── context_switch.s
└── src/
    ├── lib.rs
    ├── irq.rs               # IRQ framework and dispatch
    ├── platform.rs          # Platform service registration
    ├── wl_currency.rs       # W/L currency system
    └── scheduler/
        ├── mod.rs           # Public scheduler API
        ├── scheduler.rs     # Core scheduler implementation
        ├── task.rs          # Task struct, task management
        ├── fate_api.rs      # Fate/roulette logic
        ├── kthread.rs       # Kernel threads
        ├── ffi_boundary.rs  # Context switch support
        └── test_tasks.rs    # Test infrastructure
```

**Target structure (after Phase 2 complete):**
```
core/
├── Cargo.toml
├── context_switch.s
└── src/
    ├── lib.rs
    ├── irq.rs
    ├── platform.rs
    ├── syscall_services.rs  # NEW: Callback registrations for driver services
    ├── wl_currency.rs
    ├── scheduler/
    │   └── ...
    └── syscall/             # NEW: Moved from drivers
        ├── mod.rs
        ├── dispatch.rs
        ├── handlers.rs
        ├── common.rs
        ├── context.rs
        ├── fs.rs
        ├── macros.rs
        └── types.rs
```

### 4. What Moves Where

#### From `drivers/` to `core/`:

| File | Destination | Reason | Status |
|------|-------------|--------|--------|
| `syscall.rs` | `core/src/syscall/dispatch.rs` | Kernel policy | ❌ NOT DONE |
| `syscall_handlers.rs` | `core/src/syscall/handlers.rs` | Kernel policy | ❌ NOT DONE |
| `syscall_common.rs` | `core/src/syscall/common.rs` | Syscall utilities | ❌ NOT DONE |
| `syscall_context.rs` | `core/src/syscall/context.rs` | Syscall state | ❌ NOT DONE |
| `syscall_fs.rs` | `core/src/syscall/fs.rs` | FS syscalls | ❌ NOT DONE |
| `syscall_macros.rs` | `core/src/syscall/macros.rs` | Macro helpers | ❌ NOT DONE |
| `syscall_types.rs` | **DELETE** (re-exports from abi) | Use abi directly | ❌ NOT DONE |
| `sched_bridge.rs` | **DELETE** | Core IS the scheduler | ✅ DONE |
| `wl_currency.rs` | `core/src/wl_currency.rs` | Not a hardware driver | ✅ DONE |
| `irq.rs` (dispatch logic) | `core/src/irq.rs` | IRQ framework is core | ✅ DONE |

#### From `sched/` to `core/`:

| File | Destination | Reason | Status |
|------|-------------|--------|--------|
| `scheduler.rs` | `core/src/scheduler/scheduler.rs` | Scheduler is core | ✅ DONE |
| `task.rs` | `core/src/scheduler/task.rs` | Task model is core | ✅ DONE |
| `sched_impl.rs` | **DELETE** (integrated) | No longer needed | ✅ DONE |
| `fate_api.rs` | `core/src/scheduler/fate_api.rs` | Part of scheduler | ✅ DONE |
| `kthread.rs` | `core/src/scheduler/kthread.rs` | Kernel threads | ✅ DONE |
| `ffi_boundary.rs` | `core/src/scheduler/ffi_boundary.rs` | Context switch support | ✅ DONE |
| `test_tasks.rs` | `core/src/scheduler/test_tasks.rs` | Test infrastructure | ✅ DONE |

#### Stays in `drivers/`:

| File | Reason |
|------|--------|
| `pit.rs` | Hardware driver, provides timer functions |
| `apic.rs`, `ioapic.rs` | Hardware drivers |
| `keyboard.rs`, `mouse.rs` | Input hardware drivers |
| `serial.rs` | Console hardware driver |
| `tty.rs` | TTY layer (uses core waitqueue) |
| `pci.rs`, `virtio_gpu.rs` | Device drivers |
| `random.rs` | RNG hardware driver |
| `video_bridge.rs` | Video hardware abstraction |
| `irq.rs` (handlers only) | IRQ handlers call into core |

#### Legacy Code to DELETE:

| File/Item | Reason for Deletion | Status |
|-----------|---------------------|--------|
| `drivers/src/sched_bridge.rs` | Replaced by direct `core::` calls | ✅ DELETED |
| `sched/` (entire crate) | Merged into `core` | ✅ DELETED |
| `abi/src/sched_traits.rs` | Renamed to `abi/src/fate.rs` (only contains `FateResult` now) | ✅ RENAMED |
| All `sched_bridge::*` call sites | Replaced with `slopos_core::` calls | ✅ DONE |
| `SchedulerServices` trait | Replaced by direct function calls | ✅ DELETED |
| `BootServices` trait | Replaced by direct function calls | ✅ DELETED |
| `init_scheduler()` / `init_boot()` | Replaced by `register_platform()` | ✅ DONE |

---

## New Dependency Graph

```toml
# core/Cargo.toml
[dependencies]
slopos-abi = { workspace = true }
slopos-lib = { workspace = true }
slopos-mm = { workspace = true }
slopos-fs = { workspace = true }
spin.workspace = true
# NOTE: NO dependency on slopos-drivers!

# drivers/Cargo.toml  
[dependencies]
slopos-abi = { workspace = true }
slopos-lib = { workspace = true }
slopos-mm = { workspace = true }
slopos-core = { workspace = true }  # Can depend on core!
spin.workspace = true

# video/Cargo.toml
[dependencies]
slopos-abi = { workspace = true }
slopos-lib = { workspace = true }
slopos-mm = { workspace = true }
slopos-core = { workspace = true }
slopos-drivers = { workspace = true }
spin.workspace = true

# boot/Cargo.toml
[dependencies]
slopos-abi = { workspace = true }
slopos-lib = { workspace = true }
slopos-mm = { workspace = true }
slopos-core = { workspace = true }
slopos-drivers = { workspace = true }
slopos-video = { workspace = true }
slopos-tests = { workspace = true }
```

**Resulting graph (acyclic):**
```
kernel -> boot, core, video, userland, drivers
boot -> core, video, drivers, mm
video -> core, drivers, mm
drivers -> core, mm, lib, abi    <-- drivers depends on core, NOT vice versa
core -> mm, fs, lib, abi         <-- core has NO dependency on drivers!
fs -> mm, lib, abi
mm -> lib, abi
lib -> abi
abi -> (nothing)
```

---

## Migration Phases

Each phase ends with a working system verified by `make boot` (with appropriate timeout).

### Phase 1: Create `core` Crate and Move Scheduler ✅ COMPLETE

**Goal**: `core` crate exists with scheduler, compiles, boots.

**Tasks**:
1. ✅ Create `core/Cargo.toml` with dependencies on `abi`, `lib`, `mm`, `fs`
2. ✅ Create directory structure under `core/src/`
3. ✅ Add `core` to workspace in root `Cargo.toml`
4. ✅ Move `sched/src/*.rs` to `core/src/scheduler/`
5. ✅ Create `core/src/platform.rs` with stub implementations (panic on call)
6. ✅ Update imports in moved files to use `crate::` 
7. ✅ Temporarily keep `sched` crate as a thin re-export of `core::scheduler` for compatibility
8. ✅ Update `drivers`, `boot`, `kernel` to depend on `slopos-core`

---

### Phase 2: Move Syscalls to Core ❌ NOT DONE

**Goal**: All syscall handling is in `core`, `drivers` no longer has syscall code.

**Tasks**:
1. ❌ Create `core/src/syscall_services.rs` with callback registrations
2. ❌ Create `drivers/src/syscall_services_init.rs` to register driver functions
3. ❌ Move `drivers/src/syscall*.rs` to `core/src/syscall/`
4. ❌ Refactor all handler functions to use service callbacks instead of direct calls
5. ❌ Update all imports
6. ❌ Remove syscall modules from `drivers/src/lib.rs`
7. ❌ Update `boot/src/idt.rs` to call `slopos_core::syscall::syscall_handle()`
8. ❌ Delete `drivers/src/syscall*.rs` files

**Verification**:
```bash
make build           # Must compile
make boot            # Syscalls must work (shell responds to commands)
```

**Legacy to remove**: 
- `drivers/src/syscall.rs`
- `drivers/src/syscall_handlers.rs`
- `drivers/src/syscall_common.rs`
- `drivers/src/syscall_context.rs`
- `drivers/src/syscall_fs.rs`
- `drivers/src/syscall_macros.rs`
- `drivers/src/syscall_types.rs`

---

### Phase 3: Implement Platform Services and Remove Bridge ✅ COMPLETE

**Goal**: Platform services work, `sched_bridge.rs` is deleted.

**Tasks**:
1. ✅ Implement full `PlatformServices` struct in `core/src/platform.rs`
2. ✅ Create `drivers/src/platform_init.rs` with real function pointers
3. ✅ Update boot sequence to call `init_platform_services()` before `core::init()`
4. ✅ Replace ALL `sched_bridge::` calls with direct `slopos_core::` calls
5. ✅ Delete `drivers/src/sched_bridge.rs`
6. ✅ Move `wl_currency.rs` from `drivers` to `core`

---

### Phase 4: Move IRQ Framework to Core ✅ COMPLETE

**Goal**: IRQ dispatch is in `core`, handlers register from `drivers`.

**Tasks**:
1. ✅ Create `core/src/irq.rs` with IRQ table and dispatch
2. ✅ Create `core::irq::register()` API for drivers to register handlers
3. ✅ Split `drivers/src/irq.rs` (keep handlers, move dispatch)
4. ✅ Update `boot/src/idt.rs` to call `slopos_core::irq::irq_dispatch()`

---

### Phase 5: Delete `sched` Crate and Legacy Traits ✅ COMPLETE

**Goal**: No legacy code remains. Clean architecture.

**Tasks**:
1. ✅ Remove `sched` from workspace in root `Cargo.toml`
2. ✅ Update all `slopos-sched` dependencies to `slopos-core`
3. ✅ Delete `sched/` directory entirely
4. ✅ Remove `SchedulerServices` and `BootServices` traits
5. ✅ Rename `abi/src/sched_traits.rs` to `abi/src/fate.rs` (only contains `FateResult`)
6. ✅ Delete any remaining compatibility shims
7. ✅ Run `cargo clippy` and fix any warnings
8. ✅ Verify no dead code remains

---

## Final State Verification

### Code Verification

- [x] `grep -r "sched_bridge" .` returns nothing
- [x] `grep -r "SchedulerServices" .` returns nothing  
- [x] `grep -r "BootServices" .` returns nothing
- [x] `grep -r "slopos-sched" .` returns nothing
- [x] `grep -r "slopos_sched" .` returns nothing
- [x] No `sched/` directory exists
- [ ] No `drivers/src/syscall*.rs` files exist *(Phase 2 not done)*
- [x] `cargo clippy` has no warnings

### Runtime Verification

- [x] `make build` succeeds
- [x] `make test` passes (interrupt test harness)
- [x] `make boot` boots to shell (15s timeout)
- [x] `make boot VIDEO=1` shows graphical output
- [x] Keyboard input works
- [x] Shell commands execute (syscalls work)
- [x] Timer interrupts fire (visible in debug output)
- [x] Task switching works (run roulette)
- [x] W/L currency tracking works

### Architecture Verification

- [x] `core` has NO dependency on `drivers` (check Cargo.toml)
- [x] `drivers` depends on `core` (check Cargo.toml)
- [x] All scheduler calls are direct function calls (no trait objects)
- [x] Platform services use function pointers, not dyn traits

---

## Success Criteria

The migration is complete when:

1. **No legacy code** - `sched` crate deleted, `sched_bridge.rs` deleted, all traits deleted ✅
2. **No trait objects in hot paths** - Timer tick, syscall dispatch use direct/function-pointer calls ✅
3. **Acyclic dependency graph** - `core` does not depend on `drivers` ✅
4. **Clear ownership** - Syscalls, scheduler, IRQ framework are in `core`; hardware is in `drivers` ⚠️ Syscalls still in drivers
5. **All tests pass** - `make test` and manual verification ✅
6. **All greps clean** - No references to old patterns remain ✅
7. **Documentation updated** - `AGENTS.md` reflects new structure *(needs update after Phase 2)*
