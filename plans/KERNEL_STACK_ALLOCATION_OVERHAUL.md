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

### Phase 3 — Remove the fixed-task ceiling (~1 week)

**Goal**: task count limited by physical RAM + VA space, not a compile-time const.

1. Remove `MAX_TASKS` as a hard bound in `abi/src/task.rs`
2. Replace `[Task; MAX_TASKS]` static array in `TaskManagerInner` with a
   dynamic collection (heap-backed Vec, or slab allocator)
3. Update scheduler bookkeeping to not assume a fixed array size
4. Stress test: create 10k tasks, close them, verify no leaks

**Exit criteria**:
- 10k+ concurrent tasks supported
- Task table grows dynamically under demand
- No fixed ceiling in any data structure

### Phase 4 (optional, later) — SafeStack hardening

Fuchsia-style dual-stack design. Return addresses + register spills on a
protected stack, data on a separate one. Defeats ROP attacks by design.

Requires compiler integration (`-Z sanitizer=safestack` or similar). Defer
until Phases 1-3 are stable.

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
- [ ] Phase 3 — remove MAX_TASKS hard bound
- [ ] Phase 4 — SafeStack hardening (optional)
