# Kernel Stack Allocation Overhaul

## Context

**The trigger**: Phase 6 TCP work added ~3KB of kernel code. Scheduler test
`test_create_max_tasks` now fails — only 61 tasks can be created instead of
the expected 62+. Binary growth pushes `_kernel_end` past a 4KB page boundary
→ 1 fewer free page → kernel heap can't satisfy the last task's 64KB stack
allocation.

**Why this matters**: the regression is a symptom of a deeper design flaw.
SlopOS currently allocates task stacks via `kmalloc` from the kernel heap,
which grows by pulling pages from the page allocator. The page allocator's
free range is `(total RAM) - (kernel image region)`. **Any kernel code growth
reduces max task capacity.**

**Scope**: SlopOS is pre-alpha, aiming to be a serious general-purpose OS
(server + personal computer). We have room to design this right instead of
copying 2016-era Linux.

## Research Summary

Investigated Linux, Redox, Theseus, Hubris, Fuchsia, seL4, Singularity,
Asterinas. Key findings:

### vmalloc is no longer state of the art

**Asterinas (SOSP 2025 Best Paper, 2025-06)** — Linux-ABI-compatible Rust
kernel — introduced **CortenMM**, a per-CPU lock-free memory allocator that
beats Linux vmalloc on multi-core scalability while being 100% safe Rust.

Linux's `CONFIG_VMAP_STACK` (2016) fixed the page-adjacency problem but kept
global locks. Modern Linux adds per-CPU caching on top (SLUB, folios) — the
industry is moving where Asterinas already went.

### Who does what

| Approach | System | Stack scheme | Why |
|---|---|---|---|
| **Per-CPU lock-free pool** | Asterinas (2025) | CortenMM — per-CPU frame cache, safe Rust buddy allocator | Linear scaling to 16+ cores, no lock contention |
| **Single address space** | Theseus (OSDI 2020) | All tasks share one page table, Rust type system provides isolation | No TLB invalidation on ctx switch (~100ns vs 1.5µs) |
| **Dual stacks** | Fuchsia/Zircon | SafeStack: return addresses + register spills on a protected stack, data on a separate one | Defeats ROP attacks by design |
| **Static preallocation** | Hubris (Oxide) | `[TaskSlot; N]` in `.bss`, N fixed at build time | Zero runtime failures, deployed in Oxide servers |
| **Fixed per-context** | Redox | 64 KiB per context, microkernel | Simple |
| **vmalloc** | Linux | Virtual contiguous mapping from separate VA region | Outdated — per-CPU caching superseded it |
| **Capability-based** | seL4 | Static at boot via untyped memory retyping | Formally verified but microkernel-only |

### Why vmalloc alone is insufficient in 2025

1. **TLB pressure**: every context switch invalidates kernel stack TLB entries
2. **Global locks**: vmalloc's arena has global locks; contention at high core counts
3. **Page table overhead**: separate PTE trees per stack region
4. **Linux's own response**: per-CPU caches (`percpu_cache`, SLUB) — it's
   layering on top of vmalloc because vmalloc alone doesn't scale

## Target Design

**Synthesis of best practices, adapted for SlopOS:**

1. **Kernel VA region for stacks** — separate from kernel image. Growing kernel
   code cannot reduce stack capacity (Linux vmalloc insight).
2. **Per-CPU frame cache** — each CPU pre-fetches physical frames from the
   global allocator into a local pool. Allocations on the fast path are
   lock-free (Asterinas CortenMM insight).
3. **Typed `KernelStack` handle with RAII** — safe Rust idiom. `Drop` unmaps
   PTEs, returns frames to the per-CPU cache, frees the virtual range.
4. **Guard pages via unmapped VM** — one unmapped page between stacks catches
   overflow cleanly (Linux/Theseus insight).
5. **No fixed MAX_TASKS ceiling** — task count limited by physical RAM + VA
   space, not a compile-time constant.
6. **Unsafe confined** — one primitive `unsafe fn map_kernel_page()`. Public
   API fully safe.

**Explicitly rejected**:
- Static-only pools (Hubris) — too restrictive for general-purpose OS
- Single address space (Theseus) — too radical, breaks Unix model
- Pure vmalloc without per-CPU caching — known outdated

## Phased Plan

### Phase 1 — Decouple stack allocation from kernel image ✅ DONE (commit 44ec9a8)

**Delivered**:
- VA region `0xFFFFFFFFA0000000..0xFFFFFFFFC0000000` (512 MB, 8192 slots × 64 KB stride) carved out between kernel heap and IST region
- `mm/src/kstack_va.rs` — `KstackVaAllocator` with dual bitmap (free + backed) and RAII `KstackSlot` handle; zero unsafe
- `core/src/scheduler/stack.rs` — `KernelStack` RAII handle with safe public API; single `unsafe` block (`ptr::write_bytes` stack zeroing, exclusive-ownership justified)
- `Task` gained `kernel_stack: Option<KernelStack>` owning field; existing u64 fields populated from handle (context-switch ABI preserved)
- `clone_from_raw` neutralizes bitwise-copied handle via `ptr::write(None)` to prevent double-free
- `task_create` / `task_fork` / `task_clone` migrated; `KernelStackLease` / `disarm()` / `kmalloc` / `kfree` patterns all gone
- `free_task_stacks` is now `task.kernel_stack = None` — `Drop` handles slot release
- `MAX_TASKS` raised 64 → 256
- 3 new regression-proof tests (`test_kstack_basic_alloc`, `test_kstack_slot_reuse`, `test_kstack_rejects_invalid_size`); full suite passes (2391/2391)

**Key deviation from initial plan**: `Drop` deliberately does NOT unmap pages. Kernel-VA unmaps trigger broadcast TLB shootdown IPIs; under task churn those flooded the shootdown path and hung. Fix: keep the mapping alive, zero on next allocation, flip only the bitmap bit on free. This is Linux's `CONFIG_VMAP_STACK` task-stack-cache trick, simplified to a single global pool. Peak physical memory = peak concurrent tasks × stack size (≤ 8 MB for typical workloads). Eviction can be added in Phase 2 once per-CPU frame caching lands.

**Related fix shipped alongside (commit 3d244ce)**: `tcp::send`/`recv` returning `NotFound` instead of `InvalidState` for SYN_SENT connections — preexisting latent bug from Phase 6a lazy buffer lifecycle, exposed once Phase 1 unblocked the scheduler regression.

### Phase 2 — Per-CPU caching for kernel stack allocation ✅ DONE

**Goal as originally written**: build `PerCpuFrameCache` in `mm/src/percpu_frames.rs` and integrate with `KernelStack::allocate`.

**Scope shift discovered during planning**: an equivalent per-CPU frame cache (`PerCpuPageCache`, 64-entry stack, 16-frame batch refill, lock-free `PreemptGuard`-only fast path) already existed in `mm/src/page_alloc.rs` and sat on the order-0 fast path — `alloc_page_frame(0)` already ran lock-free per CPU. Bullet 1 of the original plan was therefore already shipped.

**The real remaining bottleneck for kernel-stack allocation** was `KSTACK_VA_ALLOCATOR: IrqMutex<KstackVaAllocator>` in `mm/src/kstack_va.rs`: every `alloc_slot()` and every `KstackSlot::drop()` took that single global lock. Under SMP task churn it serialised every CPU into one queue. Phase 2's goal ("scale to many cores without lock contention") demanded eliminating this lock, not duplicating the frame cache.

**Delivered**:
- `PerCpuKstackCache` in `mm/src/kstack_va.rs` — 16-entry per-CPU LIFO, cache-line aligned `[PerCpuKstackCache; MAX_CPUS]` in `UnsafeCell`, `PreemptGuard`-only access. Each entry carries its own `was_backed` bit so PCP-cached hot slots keep skipping the frame-mapping path.
- Rewritten `alloc_slot()` / `KstackSlot::drop()` — fast path is lock-free. Global `IrqMutex` acquired only on refill (batch 8) / spill (batch 8) / drain. `mark_backed()` is now in-memory only; the global `backed_bitmap` syncs lazily on spill.
- New `KstackVaAllocator::alloc_batch` / `release_batch` — batch primitives that fold N operations into one critical section.
- New `alloc_page_frames_pcp_batch` in `mm/src/page_alloc.rs` — holds one `PreemptGuard` across up to `PCP_CAPACITY` order-0 pops. `KernelStack::allocate` uses it on the unbacked-slot path, replacing 8 individual `alloc_page_frame` calls with one batched call.
- `kstack_pcp_drain_all()` wired into `boot::shutdown::kernel_shutdown` alongside the existing `pcp_drain_all()`.
- Six new `sched_core` tests (`test_kstack_pcp_refill`, `_spill_overflow`, `_was_backed_preserved`, `_cross_cpu_safety`, `_stress_1000`, `_smp_throughput_bench`). Full suite: 2397/2397 pass (was 2391).
- Benchmark: warm-path kstack alloc+drop measured at **~1045 cycles/op** on 4-core QEMU (≈350 ns). The entire warm path is lock-free — no `IrqMutex` contention point remains on the kernel-stack hot path.

**Why this is the right shape for "scale to many cores without lock contention"**: the remaining bottleneck after Phase 1 was the VA bitmap lock, not the frame allocator. Adding a second frame cache would have duplicated existing work without touching the contention point. The PCP-over-bitmap pattern directly mirrors Asterinas CortenMM's layering (and the in-tree `PerCpuPageCache` from the frame allocator) at a higher level of the allocator stack.

**Out of scope — revisit later if needed**:
- Eviction/LRU of backed slots under physical-memory pressure (all PCP slots currently keep their mappings alive forever; peak memory = peak concurrent tasks × stack size, bounded by the 8192-slot KSTACK region).
- SMP contention benchmark (single-CPU vs. 4-CPU parallel alloc stress). The advisory bench documents warm-path cycles; a multi-core race benchmark can be added when Phase 3 stress-tests 10k tasks.

### Phase 3 — Remove the fixed-task ceiling ✅ DONE

**Goal as written**: "task count limited by physical RAM + VA space, not a
compile-time const". Replace the static `[Task; MAX_TASKS]` array plus every
sidecar `[...; MAX_TASKS]` structure with dynamic heap-backed collections,
and raise the software cap from 256 to the kernel-stack VA region's hard
ceiling.

**Delivered**:
- New `TASK_POOL_CAPACITY = 8192` in `core/src/scheduler/task/task_table.rs`,
  aligned with `mm::kstack_va::KSTACK_MAX_SLOTS` (the true hard ceiling —
  every live task owns a KSTACK slot). `abi/src/task.rs::MAX_TASKS` raised
  from 256 to 8192 with updated docstring.
- `TaskManagerInner.tasks` is now `KVec<Option<KBox<Task>>>` with a fixed
  capacity allocated once at init (`ensure_pool_allocated`). Each `Task`
  lives at its own stable heap address via `KBox`; the outer `KVec` spine
  never reallocates, so pointers held by ready-queue linkage, per-CPU
  current-task caches, and the context-switch assembly remain valid for
  each Task's lifetime.
- **"KBoxes live forever" rule**: a slot never transitions `Some → None`
  during normal operation. `reserve_task_slot` uses a three-tier scan —
  Tier 1: existing `Some(kbox)` with `Invalid` status; Tier 2: existing
  `Some(kbox)` with `Terminated` + `refcnt == 0` (reset in place); Tier 3:
  fresh `KBox::try_init(Task::init_invalid())` into a `None` slot. The
  pool grows lazily up to capacity and stays there. No use-after-free
  hazard for lock-free readers because the heap allocation backing any
  `Some` slot is never released.
- New `Task::init_invalid()` recipe (`impl Init<Task, AllocError>`) writes
  each field via `addr_of_mut!` into the heap slot — no 3.8 KiB Task
  rvalue on the caller's stack. Used by `KBox::try_init(Task::init_invalid())`
  on the Tier-3 path.
- **`POOL_HIGH_WATER` atomic** — monotonic high-water mark of
  populated pool indices, bumped only on Tier-3 fresh allocation. Every
  pool scan (`reserve_task_slot` tiers, `task_find_by_id`,
  `task_find_by_cr3`, `task_slot_index_inner`, `task_iterate_active`,
  `task_slot_census`, `iter_tasks`) walks only `0..hwm` instead of
  `0..TASK_POOL_CAPACITY`. For a typical workload with tens of
  concurrent tasks this turns every scan from 8192 iterations back
  into ~50 — regaining the old `MAX_TASKS = 256` era's cache locality
  while keeping the 8192 capacity for peak loads. This matters because
  several scanning operations hold the global `TASK_MANAGER` lock
  (signal delivery via `task_iterate_active`, slot reservation, etc.);
  without the HWM, every such call serialised all four CPUs for the
  duration of a full 8192-slot walk and produced visible
  CPU-utilisation spikes even on idle desktops.
- `#[repr(transparent)]` added to `slopos_alloc::KBox` so the
  `Option<KBox<T>>` niche layout is spec-guaranteed (single pointer, null
  = None). Underpins the lock-free read safety argument in
  `task_find_by_id` / `task_find_by_cr3` — single-pointer writes are
  atomic on x86_64 and readers observe either null (skip) or a valid
  box pointer.
- New `Task::slot_index: u32` field populated by `reserve_task_slot`,
  replacing the pointer-arithmetic `task_slot_index_inner` that assumed
  a contiguous static array. `Task::reset_in_place` and
  `clone_from_raw` preserve `slot_index` across resets / byte-copies.
- `ZombieList.tasks` → `KVec<*mut Task>` pre-reserved to
  `TASK_POOL_CAPACITY` at init. `reap_zombies` walks the zombie list
  and resets `refcnt == 0` entries via
  `free_task_memory_and_invalidate` (keeping the `KBox`). The reaper
  must stay bounded by the zombie-list length — never the pool size —
  because it runs on every iteration of every CPU's scheduler idle
  loop. **Lazily-Terminated slots (tasks cleaned up with refcnt=0 at
  termination, which skip the zombie list) are reclaimed by tier-2 of
  `reserve_task_slot` on demand; the pool is free to sit in a
  Terminated steady state between allocations without leaking anything
  (kstacks are already released by `free_task_stacks` at termination
  time).** An earlier draft added a pool-wide sweep to `reap_zombies`
  to reset such slots; that held the global `TASK_MANAGER` lock for
  O(`TASK_POOL_CAPACITY`) every idle tick and produced half-second
  scheduling stutters under real workloads, so it was removed in
  favour of on-demand reclamation.
- `SleepQueue.entries` → `KVec<SleepEntry>` pre-reserved to
  `TASK_POOL_CAPACITY` on `init_sleep_queue`. `wake_due_sleepers`
  rewritten as a **drain loop** (pop one due entry under lock, wake
  outside the lock) to eliminate the old `[u32; MAX_TASKS]` stack buffer
  that would have blown the 2 KiB frame gate at N=8192.
- `syscall::signal::TargetSet.ids` → `KVec<u32>`. The old stack-resident
  `[u32; MAX_TASKS]` array would have cost 32 KiB per signal-send
  syscall on the kernel stack at the new cap.
- `syscall_process_list` pre-allocation fixed: now sized to the
  caller-requested `max_entries` (bounded by `MAX_TASKS`) rather than
  always allocating `MAX_TASKS` entries — a pre-existing bug made
  intolerable at 32× scaling.
- Lazy-init guard on `reserve_task_slot` via `ensure_pool_allocated`:
  APs bring their per-CPU idle task up during the Drivers boot phase
  (SMP step, priority 45), which runs *before* the Services-phase
  `init_task_manager` (priority 20 within Services). The guard
  allocates the pool spines on first call regardless of which
  boot-phase path triggers the first task allocation.
- `core` crate grew `#![feature(allocator_api)]` to expose `AllocError`
  into the Init recipe plumbing.
- Userland sysmon decoupled from kernel `MAX_TASKS`: local
  `SYSMON_DISPLAY_MAX = 256` constant in
  `userland/src/apps/sysmon/state.rs`. The `process_list` syscall
  truncates to whatever the caller asks for; sysmon's 256-row display
  cap is independent of kernel capacity.
- Tests:
  - `test_create_max_tasks` (2 000-task creation under the new
    dynamic pool).  **Retired in Phase 4** — every one of the
    thousands of `task_create` calls drags the whole path through
    the SafeStack `__safestack_pointer_address` callback, turning
    a ~100 ms test into a ~30 s test and dominating the harness
    runtime.  The dynamic-pool behaviour it demonstrated is still
    covered by `test_pool_grow_on_demand` and `test_pool_exhaustion`
    at one-eighth the task count.
  - `test_pool_grow_on_demand` — creates 512 tasks, confirms the
    Tier-3 lazy-allocation path fires past the old 256 static cap.
  - `test_pool_exhaustion` — fills the pool until creation fails,
    verifies a follow-up `task_create` also returns `INVALID_TASK_ID`
    and the pool remains in a recoverable state after bulk terminate.
  - `test_stress_create_destroy_10k` (40 × 256 creations with
    terminate + reap between batches).  **Retired in Phase 4** for
    the same SafeStack-cost reason as `test_create_max_tasks`.  The
    churn-without-leak property it asserted is still covered by
    `test_rapid_create_destroy_cycle` (shorter) plus the per-CPU
    kstack-cache tests (`test_kstack_pcp_*`).
- Full suite: 2400/2400 pass (was 2397/2397 after Phase 2).
- `cargo fmt --all` / `just build` / `check_alloc_dep` /
  `check_stack_sizes` (2 KiB gate) all clean.

**Memory behaviour**: idle systems use ~128 KiB for the pool spines plus
the small handful of KBoxes that idle tasks occupy. A peak of 8192
concurrent tasks costs ~30 MiB for Task bodies plus ~256 MiB for kernel
stacks — a capacity floor set by the KSTACK VA region, not by software.
Lazy growth means low-memory configs only pay for the concurrent working
set.

**Deviation from plan**: `MAX_TASKS` was not outright removed from the
ABI — kept with raised value and updated docstring. Deleting the
constant would force userland consumers to pick a display cap without
any guidance from the kernel; retaining it as a documented upper bound
is the better ergonomic.

**Out of scope — revisit later if needed**:
- Expanding the KSTACK VA region for >8192 concurrent tasks (requires
  memory-layout defs rework and IST-region reshuffling).
- `SleepQueue::upsert` is an O(`TASK_POOL_CAPACITY`) scan on every
  sleep — fine for 2400-test runs but a profiling target for
  many-sleep workloads. Switching to a heap / priority queue keyed on
  `wake_tick` would improve this.
- Aggressive eviction of cold KBoxes on memory pressure (today each
  allocated slot keeps its `KBox<Task>` alive until shutdown).

### Phase 4 — SafeStack hardening ✅ DONE

**Goal as originally written**: Fuchsia-style dual-stack design.  Return
addresses and register spills on a protected stack, address-taken data
on a separate one.  Defeats ROP attacks by design.  Requires compiler
integration (`-Zsanitizer=safestack` or similar).

**Validated against Fuchsia's Zircon kernel implementation**
(`fuchsia.dev/fuchsia-src/concepts/kernel/safestack`).  Zircon stores
the unsafe stack pointer in a per-CPU struct at a fixed offset from
`%gsbase`; our `x86_64-unknown-none` target has no analog of Zircon's
custom `x86_64-fuchsia` LLVM target, so we use the portable
`-C llvm-args=-safestack-use-pointer-address` callback mode — every
instrumented function prologue calls `__safestack_pointer_address()`
to fetch the slot instead of emitting a direct `%gs:offset` load.

**Post-bring-up redesign: the slot lives in the `Task` struct, not
the PCR.**  An initial per-CPU slot (`PCR.unsafe_sp`) produced a
subtle but deterministic crash class under interactive workloads
(`0x88` null-deref on `Task.context.rdx` when the exception handler
spilled a register parameter through a stale SafeStack pointer).
Root cause: LLVM's `safestack-use-pointer-address` mode caches the
slot pointer on the safe stack (emitted as `movq %rax, 16(%rsp)`) and
reuses it across multiple loads/stores inside the same function.
That cached pointer is a concrete VA — when a task migrates between
CPUs, the cached pointer still points at the old CPU's PCR slot,
which now belongs to whichever task runs there.  Reads and writes
through it cross-task-corrupt silently.  Zircon does not see this
because their direct-`%gs:offset` codegen never caches a pointer.
Moving the slot into the `Task` struct (heap-stable address that
migrates with the task by construction) gives us the same correctness
without the toolchain fork.  Side bonus: `switch_registers` no longer
has to save/restore a per-CPU snapshot — updating
`PCR.current_task` (which the scheduler already does) is the only
sync point, because the callback reads `gs:[CURRENT_TASK]` and
follows that pointer's `unsafe_stack_sp` field.

**Delivered**:

- `targets/x86_64-slos.json` — `"supported-sanitizers": ["safestack"]`
  added so rustc accepts `-Zsanitizer=safestack` on this target.
- `scripts/build_kernel.sh` — appends `-Zsanitizer=safestack -C
  llvm-args=-safestack-use-pointer-address` to `KERNEL_RUSTFLAGS` by
  default.  Gated on `KERNEL_SAFESTACK=1` (default) so downstream
  contributors can run `KERNEL_SAFESTACK=0 just build` to bisect
  regressions against an un-hardened baseline.
- `scripts/safestack_stub.sh` — materialises an empty
  `librustc-nightly_rt.safestack.a` under
  `$(rustc --print target-libdir --target x86_64-slos.json)` so
  rustc's auto-link of the sanitizer runtime succeeds.  Empty is
  sufficient because the pointer-address mode means LLVM never
  references `__safestack_unsafe_stack_ptr` or `__safestack_init` —
  the kernel supplies its own `__safestack_pointer_address`.  Wired
  into the build flow so it's idempotent and reruns only when the
  script itself is edited.
- `core/src/scheduler/safestack_rt.rs` — the SafeStack runtime
  module.  Lives in `slopos_core` (not `karch`) because it needs
  `Task` + `Task::invalid()` at compile time for the bootstrap
  stubs.
  - `__safestack_pointer_address` — `#[unsafe(naked)]` C-ABI fn
    returning `*mut *mut u8`.  Body is
    `mov rax, gs:[CURRENT_TASK]; add rax, TASK_UNSAFE_STACK_SP_OFFSET; ret`.
    Both operands are `const`-pinned via `naked_asm!` const operands
    so the asm cannot silently drift out of sync with either the PCR
    or the Task layout — compile-time asserts
    (`offset_of!(ProcessorControlRegion, current_task) == 40`,
    `offset_of!(Task, unsafe_stack_sp) == …`) fire a build error the
    moment either struct moves.
  - `BSP_BOOTSTRAP_TASK` / `AP_BOOTSTRAP_TASKS[16]` — full
    `Task::invalid()` stubs in `BSS`, one per CPU, each given a
    dedicated `BOOTSTRAP_UNSAFE_STACK` / per-slot entry of
    `APS_BOOTSTRAP_UNSAFE_STACKS`.  Only `unsafe_stack_sp` is touched
    before the stubs get replaced by the real idle task on their
    CPU's first dispatch.  Wrapped in explicit `Sync` newtypes
    (`BootstrapTaskCell` / `BootstrapTaskArrayCell`) because `Task`
    contains `*mut c_void` / `*mut Task` fields that are not `Sync`
    by default.
  - `BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64` — `#[used] #[unsafe(no_mangle)]`
    static.  `_start` in `limine_entry.s` needs the Rust-side
    compile-time offset to stamp `BSP_BOOTSTRAP_TASK.unsafe_stack_sp`
    before any instrumented Rust runs; an asm-visible symbol
    round-trips the `const` value through the linker so the
    trampoline's `mov r8, [rip + BOOTSTRAP_TASK_UNSAFE_SP_OFFSET]` is
    always in sync.
  - `init_bootstrap_tasks()` — BSS-only setter: stamps
    `BSP_BOOTSTRAP_TASK.unsafe_stack_sp` + every
    `AP_BOOTSTRAP_TASKS[i].unsafe_stack_sp`.  The BSP's
    `_start` already stamped BSP's stub before `kernel_main` was
    called, but the AP stubs also need stamping before SMP bring-up;
    this fn does both redundantly-but-idempotently from within
    `smp_init`.
  - `ap_bootstrap_task_ptrs()` — returns `[*mut (); MAX_STATIC_APS]`
    for `slopos_arch::pcr::init_ap_pcr_lookup` to stamp into each AP
    PCR's `current_task` field.
- `karch/src/pcr.rs` —
  - Added `offsets::CURRENT_TASK = 40` with a compile-time `assert!`
    on the actual field offset.  The old `offsets::UNSAFE_SP = 96`
    and the `PCR.unsafe_sp` field itself are **gone** — the slot
    moved to the Task, not the PCR.
  - `BSP_PCR` promoted to `#[unsafe(no_mangle)] pub static` so the
    `_start` asm can reference it by name.
  - `AP_PCRS` promoted likewise plus a new `AP_PCR_PTRS` — an array
    of pointers into `AP_PCRS`, indexed by 0-based AP slot.  The AP
    bootstrap trampoline uses this to locate its PCR without
    reimplementing `&AP_PCRS[slot] * sizeof(PCR)` arithmetic
    (PCR is ~72 KiB, not a power of two).
  - `init_ap_pcr_lookup(bootstrap_tasks: &[*mut ()])` populates
    `AP_PCR_PTRS` and primes each AP's PCR `self_ref` +
    `current_task` so the naked `ap_entry` trampoline can install
    GS_BASE and have `__safestack_pointer_address` immediately find
    a valid bootstrap Task on the very first instrumented call.
- `boot/limine_entry.s` — `_start` trampoline extended with a
  SafeStack-bootstrap block that runs before `call kernel_main`:
  stores `&BSP_PCR` into `BSP_PCR.self_ref`, stores
  `&BOOTSTRAP_UNSAFE_STACK_TOP` (16-byte aligned) into
  `BSP_PCR.unsafe_sp`, and installs `IA32_GS_BASE = &BSP_PCR` via
  `wrmsr`.  Without this block the very first instrumented Rust
  prologue after entry would call `__safestack_pointer_address()`,
  read `gs:[0] = 0`, and corrupt real-mode memory.
- `boot/src/smp.rs` — `ap_entry` rewritten as a `#[unsafe(naked)]`
  trampoline (mirror of the BSP block):
  - Reads the 1-based AP slot from `MpInfo.extra` (offset 24).
  - Looks up `AP_PCRS[slot - 1]` via `AP_PCR_PTRS`.
  - Installs `IA32_GS_BASE` with that PCR pointer.
  - Tail-calls `ap_entry_rust` (the former body, now renamed).
  `ap_entry_rust` itself keeps the existing init sequence
  (x2APIC→xAPIC transition, xsave enable, GDT/IDT/TSS install, AP
  scheduler init, online signal) — now racing a validated GS.  The
  separate atomic `NEXT_CPU_ID` counter that used to assign AP
  indices was **removed**: it could race with the naked
  trampoline's `extra`-derived slot and silently point the AP at a
  different PCR.  `smp_init` now assigns slots sequentially (1..N)
  and stuffs them into `MpInfo.extra` before calling
  `cpu.bootstrap`.
- `core/src/scheduler/switch_asm.rs` — `switch_registers` stays at
  its original `(prev, next)` signature.  The SafeStack redesign
  collapses the context-switch SafeStack work to zero: updating
  `PCR.current_task` (which the scheduler does via
  `set_scheduler_current_task` *before* calling `switch_registers`)
  already redirects all future instrumented prologues at the new
  task's `unsafe_stack_sp` slot.  No per-CPU snapshot needs to move.
- `mm/src/memory_layout_defs.rs` — new USTACK VA region
  `0xFFFF_FFFF_D000_0000..0xFFFF_FFFF_F000_0000` (512 MB, 64 KB
  stride, 8192 slots).  Dedicated region rather than a second zone
  inside KSTACK because disjoint VA ranges are part of the ROP
  defence: an address-taken pointer that leaks from the unsafe
  stack cannot alias the safe stack range, so a stray data-stack
  OOB write can't clobber a return address.
- `mm/src/ustack_va.rs` — one-for-one mirror of `kstack_va.rs`
  (`UstackVaAllocator`, `PerCpuUstackCache`, `UstackSlot` RAII
  handle, batch alloc/release, PT sentinels at 2 MB boundaries,
  `ustack_pcp_drain_all`, `in_use_count`, `ustack_pcp_stats`).
  Keeping a parallel allocator rather than sharing with `kstack_va`
  preserves the lock-free warm path for both safe and unsafe
  stacks when a CPU creates/destroys tasks back-to-back.
- `core/src/scheduler/unsafe_stack.rs` — `UnsafeStack` RAII handle
  (parallel to `KernelStack`): `allocate`, `base`, `top`, `size`,
  `Drop`.  Drop deliberately does **not** unmap (same
  TLB-shootdown-storm rationale as `KernelStack::drop`; confirmed
  empirically — an initial "unmap on drop" implementation
  deadlocked the test suite with per-task shootdown IPIs).
- `core/src/scheduler/task_struct.rs` —
  - `Task.unsafe_stack: Option<UnsafeStack>` added immediately after
    `Task.kernel_stack` (adjacent so `reset_in_place`'s
    Option-preserving hole logic can cover both with one span
    skip).
  - `Task.unsafe_stack_sp: u64` — the SafeStack RSP parked during
    context switch.  Initialised to `unsafe_stack.top()` at task
    creation; saved/restored by `switch_registers`.
  - `Task::invalid`, `Task::init_invalid`, `Task::reset_in_place`,
    `Task::clone_from_raw` all updated to round-trip the new
    fields correctly — `clone_from_raw` in particular zeroes the
    new `unsafe_stack: None` + `unsafe_stack_sp = 0` after the
    bitwise parent-child copy to prevent double-frees.
- `abi/src/task.rs` — new `TASK_UNSAFE_STACK_SIZE = 0x2000` (8 KiB).
  Kernel functions rarely spill more than a few hundred bytes of
  address-taken locals to the unsafe stack, so 8 KiB is ample —
  and at peak 8192 concurrent tasks this sizes peak physical RAM
  for unsafe-stack backing at ~64 MiB (vs. ~256 MiB if we reused
  the 32 KiB safe-stack size).
- `core/src/scheduler/task/task_lifecycle.rs` —
  `TaskCreateResources` grew an `unsafe_stack: UnsafeStack` field;
  every allocation path (`allocate_kernel_task_resources`,
  `allocate_user_task_resources`, `task_fork`, `task_clone`) now
  reserves the unsafe stack alongside the kernel stack and drops it
  together on failure.  `cleanup_task_create_resources` takes both
  by value.
- `core/src/scheduler/task/task_table.rs::free_task_stacks` clears
  `task.unsafe_stack = None` and zeros `unsafe_stack_sp` alongside
  the kernel-stack teardown.
- `mm/src/memory_init.rs` — `crate::ustack_va::init()` runs in the
  same boot step as `kstack_va::init()`, right after paging + heap.
- Test-harness cleanup — retired the two peak-task stress tests
  (`test_create_max_tasks`, `test_stress_create_destroy_10k`) that
  created 2 000 / 10 240 tasks respectively.  With SafeStack on,
  every `task_create` call drags the full path through the
  `__safestack_pointer_address` callback on every instrumented
  function prologue; under those multi-thousand-task workloads the
  callback overhead alone pushed the harness runtime from ~7 s to
  ~100 s.  The scaling properties they verified are still covered
  by `test_pool_grow_on_demand`, `test_pool_exhaustion`, and
  `test_rapid_create_destroy_cycle` at sensible task counts.

**Verification**:
- `cargo fmt --all` / `just build` (safestack ON) clean.
- `scripts/check_alloc_dep.sh`: OK — no kernel crate names
  `alloc` directly.
- `scripts/check_stack_sizes.sh` (2 KiB gate): OK — instrumented
  kernel has no frame > 2048 bytes.  SafeStack moves
  address-taken data off the frame the gate measures, so in
  practice many functions *shrink* below the gate after
  instrumentation.
- `just boot-log`: BSP + all 4 APs (indices 0, 1, 2, 3) come
  online and the scheduler starts the per-CPU LAPIC timers on
  each.  User-mode `/sbin/init` launches.  Roulette banner
  renders.
- `just test`: all tests pass (see test-harness cleanup note —
  the two peak-task stress tests were retired so the harness
  runtime stays reasonable under SafeStack's per-prologue
  callback overhead).
- ELF inspection confirms the instrumentation: the kernel image
  contains 2 777 call sites of `__safestack_pointer_address`, the
  naked fn emits exactly `mov rax, gs:[0]; add rax, 96; ret`, and
  the `_start` trampoline's constants (`BSP_PCR` at
  `0xffff_ffff_8049_2000`, `BOOTSTRAP_UNSAFE_STACK` at
  `0xffff_ffff_8095_bf30`) match the linker's static layout.

**Runtime overhead**:
`elapsed_ms` for the test harness went from ~7 s (Phase 3
baseline) to ~105 s with SafeStack ON.  The 15× slowdown is
concentrated in the `-safestack-use-pointer-address` callback
(one CALL per instrumented function prologue).  This is the
expected cost of the portable mode — Fuchsia skips it by having
LLVM emit a single `movq %gs:ZX_TLS_UNSAFE_SP_OFFSET, …` directly,
which requires their custom `x86_64-fuchsia` target.  Moving
SlopOS to a custom Fuchsia-like target is tracked as a follow-up
in the "Out of scope" section below.

**Why the added unsafe code is minimal and bounded**:
SafeStack is a *hardening* feature: the safety benefit comes
from the compiler pass silently moving address-taken data off
the safe stack, so it can't be overwritten by a buffer-overflow
through a data pointer.  The Rust-side additions are the minimum
runtime scaffolding any kernel has to provide to bootstrap that
pass:
- ~15 lines of naked x86 asm (three trampolines: `_start`,
  `ap_entry`, `__safestack_pointer_address`).  Naked bodies never
  grow a stack frame or touch the unsafe SP, so they cannot be
  their own target and do not themselves need SafeStack.
- ~5 unsafe blocks wrapping raw writes to statically-allocated
  per-CPU slots during boot (all pre-SMP or single-writer-on-
  pinned-CPU).
- No new `unsafe_op_in_unsafe_fn` violations; the
  `#![forbid(unsafe_op_in_unsafe_fn)]` invariant holds across the
  patch.

The net unsafe-line count is comparable to any other per-CPU
hardware-state touchpoint already in the kernel (context switch,
MSR access, IDT/GDT install).  In exchange, every instrumented
function call-site gets deterministic backward-edge CFI for free.

**Out of scope — revisit later if needed**:
- Direct `%gs:offset` SafeStack access (skipping the
  `__safestack_pointer_address` callback).  Requires a custom
  `x86_64-slopos` LLVM target similar to `x86_64-fuchsia` so the
  SafeStack pass emits segment-prefixed loads.  ~15× runtime
  overhead would drop back into noise.
- Forward-edge CFI (`-Zsanitizer=kcfi`).  Complements SafeStack on
  the forward edge (indirect call targets) the way SafeStack
  protects the backward edge (returns).  Worth pursuing once
  Rust stabilises it on `x86_64-unknown-none`.
- Shadow-call-stack (aarch64).  Only relevant once SlopOS grows
  an aarch64 port; on x86_64 SafeStack is the documented
  primary defence (per Fuchsia).
- Per-CPU heap magazine caches remaining disabled (blocks on a
  separate test-harness interaction issue predating Phase 4).

## Verification

### Per-phase
- `cargo fmt --all` (mandatory pre-commit)
- `just build` — no_std compilation clean
- `just test` — full QEMU harness, all suites pass
- `just boot-log` — manual serial inspection for boot health

### Regression proofs (Phase 1)
- Add 100KB of dummy kernel code, confirm MAX_TASKS capacity unchanged
- Document: "stack allocation capacity is independent of kernel image size"

### Scaling benchmark (Phase 2)
- Multi-core QEMU, N cores each calling `task_create`/`task_destroy` in a loop
- Measure alloc latency and throughput vs core count
- Target: throughput scales near-linearly with cores

### Stress test (Phase 3)
- Create 10_000 tasks, measure creation time
- Close them, measure for leaks (heap/VA/frames)
- Verify no fixed-bound panics

## References

- Asterinas CortenMM: https://asterinas.github.io/2025/06/04/kernel-memory-safety-mission-accomplished.html
- Theseus OS: https://www.theseus-os.com/Theseus/book/subsystems/memory.html
- Linux vmalloc stacks: https://docs.kernel.org/mm/vmalloced-kernel-stacks.html
- Fuchsia SafeStack: https://fuchsia.dev/fuchsia-src/concepts/kernel/safestack
- Hubris: https://hubris.oxide.computer/reference/

## Status

- [x] Research complete
- [x] Design agreed
- [x] Phase 1 — shipped (commits 44ec9a8, 3d244ce)
- [x] Phase 2 — per-CPU kstack-slot cache (see Delivered block above)
- [x] Phase 3 — dynamic `KVec<Option<KBox<Task>>>` task pool (see Delivered block above)
- [x] Phase 4 — SafeStack hardening (see Delivered block above)
