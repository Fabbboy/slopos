# SlopOS Architecture Restructure: Introducing the `core` Crate

## Executive Summary

This plan proposes restructuring SlopOS's crate dependencies by introducing a new `core` crate that owns kernel policy (scheduler, syscalls, IRQ framework), while `drivers` becomes a pure hardware abstraction layer. This eliminates the current `sched_bridge` runtime indirection pattern and creates a clean, one-directional dependency graph.

**End State**: No legacy code remains. The `sched` crate is deleted, `sched_bridge.rs` is deleted, all `SchedulerServices`/`BootServices` traits are removed from `abi`, and all call sites use the new `core` APIs directly.

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

```
core/
├── Cargo.toml
└── src/
    ├── lib.rs
    │
    ├── scheduler/
    │   ├── mod.rs           # Public scheduler API
    │   ├── task.rs          # Task struct, task management
    │   ├── queue.rs         # Ready queue implementation
    │   ├── context_switch.rs # Context switch (calls asm)
    │   └── fate.rs          # Fate/roulette logic
    │
    ├── syscall/
    │   ├── mod.rs           # Syscall dispatch
    │   ├── dispatch.rs      # syscall_handle() entry point
    │   ├── context.rs       # SyscallContext helper
    │   ├── common.rs        # Shared utilities
    │   ├── macros.rs        # define_syscall! macro
    │   └── handlers/
    │       ├── mod.rs
    │       ├── process.rs   # yield, exit, spawn, sleep
    │       ├── memory.rs    # shm_*, mmap (calls mm crate)
    │       ├── fs.rs        # fs_* (calls fs crate)
    │       ├── video.rs     # surface_*, fb_* (calls video crate)
    │       ├── input.rs     # input_* handlers
    │       └── system.rs    # sys_info, halt, etc.
    │
    ├── irq/
    │   ├── mod.rs           # IRQ framework
    │   └── dispatch.rs      # irq_dispatch() entry point
    │
    ├── waitqueue.rs         # Wait/wake primitives
    ├── platform.rs          # Platform service registration
    ├── events.rs            # Event signaling (drivers -> core)
    ├── panic.rs             # kernel_panic implementation
    ├── shutdown.rs          # kernel_shutdown, kernel_reboot
    └── wl_currency.rs       # W/L currency (moved from drivers)
```

### 4. What Moves Where

#### From `drivers/` to `core/`:

| File | Destination | Reason |
|------|-------------|--------|
| `syscall.rs` | `core/src/syscall/dispatch.rs` | Kernel policy |
| `syscall_handlers.rs` | `core/src/syscall/handlers/*.rs` | Kernel policy |
| `syscall_common.rs` | `core/src/syscall/common.rs` | Syscall utilities |
| `syscall_context.rs` | `core/src/syscall/context.rs` | Syscall state |
| `syscall_fs.rs` | `core/src/syscall/handlers/fs.rs` | FS syscalls |
| `syscall_macros.rs` | `core/src/syscall/macros.rs` | Macro helpers |
| `syscall_types.rs` | **DELETE** (re-exports from abi) | Use abi directly |
| `sched_bridge.rs` | **DELETE** | Core IS the scheduler |
| `wl_currency.rs` | `core/src/wl_currency.rs` | Not a hardware driver |
| `irq.rs` (dispatch logic) | `core/src/irq/` | IRQ framework is core |

#### From `sched/` to `core/`:

| File | Destination | Reason |
|------|-------------|--------|
| `scheduler.rs` | `core/src/scheduler/mod.rs` | Scheduler is core |
| `task.rs` | `core/src/scheduler/task.rs` | Task model is core |
| `sched_impl.rs` | **DELETE** (integrated) | No longer needed |
| `fate_api.rs` | `core/src/scheduler/fate.rs` | Part of scheduler |
| `kthread.rs` | `core/src/scheduler/kthread.rs` | Kernel threads |
| `ffi_boundary.rs` | **DELETE** or integrate | Context switch support |
| `test_tasks.rs` | `core/src/scheduler/test_tasks.rs` | Test infrastructure |

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

| File/Item | Reason for Deletion |
|-----------|---------------------|
| `drivers/src/sched_bridge.rs` | Replaced by direct `core::` calls |
| `sched/` (entire crate) | Merged into `core` |
| `abi/src/sched_traits.rs` | No longer needed - no trait objects |
| All `sched_bridge::*` call sites | Replaced with `slopos_core::` calls |
| `SchedulerServices` trait | Replaced by direct function calls |
| `BootServices` trait | Replaced by direct function calls |
| `init_scheduler()` / `init_boot()` | Replaced by `register_platform()` |

#### IRQ Split:

The current `drivers/src/irq.rs` contains both:
1. IRQ dispatch framework (moves to `core/src/irq/`)
2. Hardware-specific handlers (stays in `drivers/`)

```rust
// core/src/irq/dispatch.rs
pub fn irq_dispatch(irq: u8, frame: *mut InterruptFrame) {
    // Call registered handler
    if let Some(handler) = IRQ_TABLE[irq as usize].handler {
        handler(irq, frame, IRQ_TABLE[irq as usize].context);
    }
    
    // Send EOI
    apic::send_eoi();  // Called via platform service
    
    // Handle post-IRQ scheduling
    scheduler::handle_post_irq();
}

// drivers/src/irq_handlers.rs
pub fn register_irq_handlers() {
    core::irq::register(0, timer_irq_handler, "timer");
    core::irq::register(1, keyboard_irq_handler, "keyboard");
    core::irq::register(12, mouse_irq_handler, "mouse");
}
```

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

## Platform Service Registration

### Driver Side (Registration)

```rust
// drivers/src/platform_init.rs

use slopos_core::platform::{PlatformServices, register_platform};
use crate::{pit, serial, random};

static PLATFORM_SERVICES: PlatformServices = PlatformServices {
    timer_get_ticks: || crate::irq::get_timer_ticks(),
    timer_get_frequency: || pit::pit_get_frequency(),
    timer_poll_delay_ms: |ms| pit::pit_poll_delay_ms(ms),
    console_putc: |c| serial::serial_putc_com1(c),
    console_puts: |s| {
        for &c in s {
            serial::serial_putc_com1(c);
        }
    },
    rng_next: || random::random_next(),
    gdt_set_kernel_rsp0: |rsp0| crate::gdt::set_kernel_rsp0(rsp0),
};

pub fn init_platform_services() {
    register_platform(&PLATFORM_SERVICES);
}
```

### Boot Sequence

```rust
// boot/src/init.rs

pub fn kernel_init() {
    // Phase 1: Hardware init (no scheduler yet)
    slopos_drivers::early_init();      // Serial, basic hardware
    slopos_mm::init();                  // Memory management
    
    // Phase 2: Register platform services
    slopos_drivers::platform_init::init_platform_services();
    
    // Phase 3: Core init (scheduler, IRQ framework)
    slopos_core::init();               // Can now use platform services
    
    // Phase 4: Full driver init
    slopos_drivers::late_init();       // PCI, virtio, etc.
    
    // Phase 5: Video, userland
    slopos_video::init();
    slopos_userland::init();
    
    // Phase 6: Enable interrupts, start scheduler
    slopos_core::scheduler::start();
}
```

---

## Rust Language Features Leveraged

### 1. Function Pointers over Trait Objects (Hot Paths)

```rust
// OLD (current approach) - DELETE THIS:
static SERVICE: Once<&'static dyn TimerService> = Once::new();
fn timer_ticks() -> u64 {
    SERVICE.get().map(|s| s.get_ticks()).unwrap_or(0)  // vtable + Option
}

// NEW (proposed):
static TIMER_GET_TICKS: AtomicPtr<fn() -> u64> = AtomicPtr::new(ptr::null_mut());
#[inline(always)]
fn timer_ticks() -> u64 {
    let f = unsafe { *TIMER_GET_TICKS.load(Ordering::Acquire) };
    f()  // Direct call, no vtable
}
```

### 2. `#[inline(always)]` for Platform Accessors

```rust
#[inline(always)]
pub fn timer_ticks() -> u64 {
    (platform().timer_get_ticks)()
}
```

### 3. Module Visibility for Encapsulation

```rust
// core/src/scheduler/mod.rs
pub(crate) mod queue;           // Only core can access queue internals
pub(crate) mod context_switch;  // Implementation detail

pub use task::Task;             // Public API
pub fn schedule() { ... }       // Public API
```

### 4. `const` Initialization

```rust
// Instead of Once<>, use const where possible
static SERVICES: PlatformServices = PlatformServices {
    timer_get_ticks: || panic!("not initialized"),
    // ... stub implementations that panic
};
static INITIALIZED: AtomicBool = AtomicBool::new(false);
```

### 5. Feature Flags for Testing

```rust
#[cfg(feature = "test-platform")]
pub fn register_test_platform(services: &'static PlatformServices) {
    // Allow tests to inject mock platform
}

#[cfg(test)]
mod tests {
    use super::*;
    
    static TEST_SERVICES: PlatformServices = PlatformServices {
        timer_get_ticks: || 12345,
        // ... mock implementations
    };
    
    #[test]
    fn test_scheduler_timing() {
        register_test_platform(&TEST_SERVICES);
        // Test scheduler behavior with controlled time
    }
}
```

---

## Migration Phases

Each phase ends with a working system verified by `make boot` (with appropriate timeout).

### Phase 1: Create `core` Crate and Move Scheduler

**Goal**: `core` crate exists with scheduler, compiles, boots.

**Tasks**:
1. Create `core/Cargo.toml` with dependencies on `abi`, `lib`, `mm`, `fs`
2. Create directory structure under `core/src/`
3. Add `core` to workspace in root `Cargo.toml`
4. Move `sched/src/*.rs` to `core/src/scheduler/`
5. Create `core/src/platform.rs` with stub implementations (panic on call)
6. Update imports in moved files to use `crate::` 
7. Temporarily keep `sched` crate as a thin re-export of `core::scheduler` for compatibility
8. Update `drivers`, `boot`, `kernel` to depend on `slopos-core`

**Verification**:
```bash
make build           # Must compile
make boot            # Must boot, scheduler works
```

**Legacy removed**: None yet (compatibility shim in place)

---

### Phase 2: Move Syscalls to Core

**Goal**: All syscall handling is in `core`, `drivers` no longer has syscall code.

**Tasks**:
1. Move `drivers/src/syscall*.rs` to `core/src/syscall/`
2. Split `syscall_handlers.rs` into `handlers/*.rs` by category
3. Update all imports
4. Remove syscall modules from `drivers/src/lib.rs`
5. Update `boot/src/idt.rs` to call `slopos_core::syscall::syscall_handle()`
6. Delete `drivers/src/syscall*.rs` files

**Verification**:
```bash
make build           # Must compile
make boot            # Syscalls must work (shell responds to commands)
```

**Legacy removed**: 
- `drivers/src/syscall.rs`
- `drivers/src/syscall_handlers.rs`
- `drivers/src/syscall_common.rs`
- `drivers/src/syscall_context.rs`
- `drivers/src/syscall_fs.rs`
- `drivers/src/syscall_macros.rs`
- `drivers/src/syscall_types.rs`

---

### Phase 3: Implement Platform Services and Remove Bridge

**Goal**: Platform services work, `sched_bridge.rs` is deleted.

**Tasks**:
1. Implement full `PlatformServices` struct in `core/src/platform.rs`
2. Create `drivers/src/platform_init.rs` with real function pointers
3. Update boot sequence to call `init_platform_services()` before `core::init()`
4. Replace ALL `sched_bridge::` calls with direct `slopos_core::` calls:
   - `sched_bridge::timer_tick()` -> `slopos_core::scheduler::timer_tick()`
   - `sched_bridge::schedule()` -> `slopos_core::scheduler::schedule()`
   - `sched_bridge::get_current_task()` -> `slopos_core::scheduler::get_current_task()`
   - `sched_bridge::kernel_panic()` -> `slopos_core::panic::kernel_panic()`
   - etc.
5. Delete `drivers/src/sched_bridge.rs`
6. Move `wl_currency.rs` from `drivers` to `core`

**Verification**:
```bash
make build           # Must compile with no sched_bridge references
make boot            # Timer ticks, scheduling, panic all work
grep -r "sched_bridge" .  # Must return nothing
```

**Legacy removed**:
- `drivers/src/sched_bridge.rs`
- All `sched_bridge::` call sites (48 locations)

---

### Phase 4: Move IRQ Framework to Core

**Goal**: IRQ dispatch is in `core`, handlers register from `drivers`.

**Tasks**:
1. Create `core/src/irq/mod.rs` with IRQ table and dispatch
2. Create `core/src/irq/dispatch.rs` with `irq_dispatch()` 
3. Create `core::irq::register()` API for drivers to register handlers
4. Split `drivers/src/irq.rs`:
   - Keep handler functions (`timer_irq_handler`, etc.)
   - Move dispatch logic to core
5. Create `drivers/src/irq_handlers.rs` with registration
6. Update boot to call `slopos_drivers::irq_handlers::register_irq_handlers()`
7. Update `boot/src/idt.rs` to call `slopos_core::irq::irq_dispatch()`

**Verification**:
```bash
make build           # Must compile
make boot            # Timer interrupts fire, keyboard works
make test            # Interrupt test harness passes
```

**Legacy removed**: Old IRQ dispatch code in drivers

---

### Phase 5: Delete `sched` Crate and Legacy Traits

**Goal**: No legacy code remains. Clean architecture.

**Tasks**:
1. Remove `sched` from workspace in root `Cargo.toml`
2. Update all `slopos-sched` dependencies to `slopos-core`
3. Delete `sched/` directory entirely
4. Delete `abi/src/sched_traits.rs` (SchedulerServices, BootServices, TaskCleanupHook)
5. Remove trait-related code from `abi/src/lib.rs`
6. Delete any remaining compatibility shims
7. Update `AGENTS.md` to reflect new crate structure
8. Run `cargo clippy` and fix any warnings
9. Verify no dead code remains

**Verification**:
```bash
make build           # Must compile with no warnings
make test            # All tests pass
make boot            # Full boot to shell works
make boot VIDEO=1    # Graphical boot works
```

**Final grep checks** (all must return nothing):
```bash
grep -r "sched_bridge" --include="*.rs" .
grep -r "SchedulerServices" --include="*.rs" .
grep -r "BootServices" --include="*.rs" .
grep -r "slopos-sched" .
grep -r "slopos_sched" --include="*.rs" .
```

**Legacy removed**:
- `sched/` crate (entire directory)
- `abi/src/sched_traits.rs`
- All trait object infrastructure

---

## Final State Verification

After all phases complete:

### Code Verification
- [ ] `grep -r "sched_bridge" .` returns nothing
- [ ] `grep -r "SchedulerServices" .` returns nothing  
- [ ] `grep -r "BootServices" .` returns nothing
- [ ] `grep -r "slopos-sched" .` returns nothing
- [ ] `grep -r "Once<&'static dyn" .` returns nothing in core/drivers
- [ ] No `sched/` directory exists
- [ ] No `drivers/src/syscall*.rs` files exist
- [ ] `cargo clippy` has no warnings

### Runtime Verification
- [ ] `make build` succeeds
- [ ] `make test` passes (interrupt test harness)
- [ ] `make boot` boots to shell (15s timeout)
- [ ] `make boot VIDEO=1` shows graphical output
- [ ] Keyboard input works
- [ ] Shell commands execute (syscalls work)
- [ ] Timer interrupts fire (visible in debug output)
- [ ] Task switching works (run roulette)
- [ ] W/L currency tracking works

### Architecture Verification
- [ ] `core` has NO dependency on `drivers` (check Cargo.toml)
- [ ] `drivers` depends on `core` (check Cargo.toml)
- [ ] All scheduler calls are direct function calls (no trait objects)
- [ ] Platform services use function pointers, not dyn traits

---

## Success Criteria

The migration is complete when:

1. **No legacy code** - `sched` crate deleted, `sched_bridge.rs` deleted, all traits deleted
2. **No trait objects in hot paths** - Timer tick, syscall dispatch use direct/function-pointer calls
3. **Acyclic dependency graph** - `core` does not depend on `drivers`
4. **Clear ownership** - Syscalls, scheduler, IRQ framework are in `core`; hardware is in `drivers`
5. **All tests pass** - `make test` and manual verification
6. **All greps clean** - No references to old patterns remain
7. **Documentation updated** - `AGENTS.md` reflects new structure
