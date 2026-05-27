---
name: SlopOS Framekernel Architecture Plan
description: Four-phase rip-and-replace plan to redesign SlopOS as an async-first framekernel with a Verus-verified OSTD critical path
status: phase-3-ready
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: **Phase 1 & 2 complete.** `slopos-ostd` owns every line of kernel `unsafe`; all 17 non-OSTD kernel crates are `#![forbid(unsafe_code)]`; TCB ratio 0.722 % (target ≤1 %). Tagged `framekernel-phase-1`; `framekernel-phase-2` close commit pending. **Phase 3 (async-first task model) is next.** Per-subphase implementation notes live in `git log`.
> **Target**: Redesign SlopOS as an **async-first framekernel** with a small, partially formally-verified trusted core (`slopos-ostd`). Pre-alpha rip-and-replace; no backwards compatibility constraints.
> **Scope**: Whole-kernel architecture. Affects every subsystem.
> **Working directory**: `/home/nil0ft/repos/slopos`
> **Headline KPI**: TCB ratio (lines of `unsafe` divided by total kernel LoC). Target ≤1% by end of Phase 2.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architectural Decisions](#2-architectural-decisions)
3. [Glossary](#3-glossary)
4. [Cross-Phase Conventions](#4-cross-phase-conventions)
5. [Phase 1 — OSTD Foundation](#5-phase-1--ostd-foundation)
6. [Phase 2 — Safe-Rust Kernel Services](#6-phase-2--safe-rust-kernel-services)
7. [Phase 3 — Async-First Task Model](#7-phase-3--async-first-task-model)
8. [Phase 4 — Verus Verification of OSTD Critical Path](#8-phase-4--verus-verification-of-ostd-critical-path)
9. [Out of Scope / Deferred](#9-out-of-scope--deferred)
10. [Risk Register](#10-risk-register)
11. [References](#11-references)

---

## 1. Executive Summary

SlopOS today is ~155K LoC of kernel Rust with ~1,426 `unsafe` occurrences (0.92%) clustered in seven subsystems. Drivers are already 0.19% unsafe — exemplary. The codebase already has proto-OSTD primitives (`slopos-alloc`, `IrqMutex`, `OwnedPageFrame`, `MmioRegion`, `UserPtr<T>`) and build-time gates (`check_alloc_dep.sh`, `check_stack_sizes.sh` at 2 KiB) that are stricter than Linux mainline.

This plan does four things, in strict serial order:

1. **Phase 1 — OSTD Foundation**: carve a single `slopos-ostd` crate that owns *every* line of `unsafe` in the kernel. Forbid `unsafe` everywhere else. Build the typed primitives (`Frame<M>`, `UFrame`, `USegment`, `VmSpace::cursor`, `IoMem`, `IoPort`, `DmaCoherent`, `IrqLine`, `UserContext`). Existing kernel is migrated to consume OSTD at parity.
2. **Phase 2 — Safe-Rust Kernel Services**: rip and replace `mm/`, `core/`, `fs/`, `drivers/`, `net/` with safe-Rust services on top of OSTD. Page allocator, slab, scheduler, syscall dispatch all become injectable trait impls *outside* the TCB. Achieve `#![forbid(unsafe_code)]` on every non-OSTD kernel crate.
3. **Phase 3 — Async-First Task Model**: tasks become `Pin<Box<dyn Future>>`. Cooperative scheduling primary, preemptive backstop. Every blocking syscall becomes `async fn`. Async TLB shootdown. **This is the differentiator** — no production Rust kernel has done it.
4. **Phase 4 — Verus Verification of OSTD Critical Path**: machine-checked proofs of `Frame<M>` ref-count, slab/`HeapSlot` lifetimes, and `VmSpace::cursor` invariants. Pinned Verus toolchain. CI-gated proof regressions.

Phases are strictly serial. Phase 1 → 2 is structural. Phase 3 invalidates proofs done before it, which is why Phase 4 is last.

---

## 2. Architectural Decisions

These decisions are **load-bearing**. Changing one means re-planning. Document every deviation in a PR description.

| # | Decision | Rationale |
|---|---|---|
| **AD-1** | Single trusted crate named `slopos-ostd`. All `unsafe` lives here. | Asterinas's OSTD pattern; Theseus's lack of architectural enforcement is exactly why its TCB is 62%. |
| **AD-2** | Every other kernel crate carries `#![forbid(unsafe_code)]`. Build gate enforces. | Discipline only works when load-bearing in CI. |
| **AD-3** | Untyped-memory abstraction (`UFrame`/`USegment`) exposes only byte-copy I/O. Never `&T` / `&mut T` over user, MMIO, or DMA memory. | The exact bug class Theseus inherited. Closing it is the single biggest soundness improvement. |
| **AD-4** | Page-table mutation only via `VmSpace::cursor`. `pml4` raw pointer is never public. | Sets up Phase 4 verification + future CortenMM-style scalable MM. |
| **AD-5** | `Frame<M: AnyFrameMeta>` carries typed per-page metadata. Page cache, slab, page table attach `M` without enlarging TCB. | Keeps OSTD flat as features grow. |
| **AD-6** | IOMMU default-deny. DMA mappings only over `UFrame`/`USegment`. | Inv. 6 (peripheral cannot tamper with sensitive memory). |
| **AD-7** | Scheduler, page allocator, slab are **injectable traits** implemented *outside* OSTD. | Linux's scheduler grew from 1.6K to 27K LoC over two decades; if it's in the TCB, the TCB drifts. |
| **AD-8** | **Async-first**. Tasks are futures. Cooperative executor primary; preemptive only as backstop. | The unique research contribution. |
| **AD-9** | OSTD itself is sync. Async lives in Phase-2 services *on top of* OSTD primitives. | Keeps the trusted core small and verifiable. |
| **AD-10** | Verus is the verifier. Pinned fork (vostd-style). | Best-fit for systems Rust; Asterinas's choice; precedent matters. |
| **AD-11** | Generation-counter handles for fds, pipes, page tables, tasks. | Hubris's idea. Stale references → typed errors, never UB. |
| **AD-12** | Target: ≤1.5% TCB ratio after Phase 1, ≤1% after Phase 2. | Asterinas: 14%. We start small enough to do better. |
| **AD-13** | x86_64 only through Phase 4. ARM64/RISC-V deferred. | Single arch keeps OSTD surface tractable. |
| **AD-14** | No CHERI/MTE integration in this plan. Design pointer types so they *can* carry tags later. | Hardware availability gates. Don't gate the project on it. |
| **AD-15** | No user-visible capability system in this plan. Phase 5+ candidate. | Smaller research delta; keep focus. |

---

## 3. Glossary

| Term | Definition |
|---|---|
| **OSTD** | Operating System Trusted Domain. The single crate (`slopos-ostd`) that owns all `unsafe`. Asterinas's term, adopted. |
| **TCB** | Trusted Computing Base. Code whose correctness is required for soundness. In this plan: `slopos-ostd` only. |
| **Untyped memory** | Memory that hardware (DMA, MMIO) or user code can mutate. Exposed via `UFrame`/`USegment` byte-copy interfaces, never as a Rust reference. |
| **`Frame<M>`** | Owned physical frame with typed metadata `M`. Replaces today's `OwnedPageFrame`. |
| **`UFrame<M>`** | Frame whose `M: AnyUFrameMeta`. Byte-copy interface only. |
| **`VmSpace`** | Per-process virtual address space. Replaces today's `ProcessPageDir` exposure. Mutation via `cursor`. |
| **Cursor** | Walking handle over a virtual range that can map/unmap `UFrame`s. Safe substitute for raw page-table edits. |
| **Inv. N** | One of the 10 framekernel soundness invariants from Asterinas paper §4.3. Reproduced in Section 5.M. |
| **Cooperative executor** | The Phase-3 async runtime. Polls futures; yields between them. |
| **Preemptive backstop** | Phase-3 timer-driven preemption that runs when a future doesn't yield voluntarily. |
| **KernMiri** | Asterinas's fork of Miri with kernel-context shims. Used for dynamic UB detection in OSTD. |
| **Verus** | SMT-backed Rust verifier. Used for static proof of OSTD critical-path invariants. |
| **TCB ratio** | `(unsafe LoC in slopos-ostd) / (total kernel LoC)`. Tracked per PR. |

---

## 4. Cross-Phase Conventions

**These rules apply to every subtask in every phase.** An agent working any task must obey them.

### 4.1 File and crate layout

- New trusted code goes in `slopos-ostd/src/`. Module structure detailed in Section 5.A.
- Non-trusted kernel code lives in existing crates (`mm/`, `core/`, `fs/`, `drivers/`, `net/`, `sync/`, `boot/`) which are *consumers* of OSTD after Phase 1.
- New crates require a justification line in the PR description (we already have 26 kernel crates; consolidation is preferred over proliferation).

### 4.2 Coding rules

- `slopos-ostd` carries `#![forbid(unsafe_op_in_unsafe_fn)]`. Every `unsafe` block needs a `// SAFETY:` comment naming which Inv. it preserves.
- Every other kernel crate carries `#![forbid(unsafe_code)]` after Phase 1. Adding `unsafe` to a non-OSTD crate is a build error.
- Four-space indent, brace-on-same-line, `pub(crate)` preferred for cross-module helpers (per `AGENTS.md`).
- Public OSTD APIs prefixed with their subsystem: `slopos_ostd::mm::`, `slopos_ostd::sync::`, `slopos_ostd::cpu::`.
- `slopos-alloc`'s discipline (`KBox`, `KVec`, `KArc`, `PinBox`, `Init<T,E>`) is preserved; no `extern crate alloc;` outside OSTD's allocator-glue module.

### 4.3 Testing

- Every subtask group ends with a `just test` invocation showing parity (no test-count regression) and explicit verification criteria.
- New OSTD primitives get unit tests in `slopos-ostd/src/<module>/tests.rs` using the existing `stest!` / `utest!` macros.
- KernMiri runs (Phase 1.K onwards) target ≥90% line coverage on `slopos_ostd::mm` and `slopos_ostd::sync`.
- The 2 KiB stack-frame ceiling from `scripts/check_stack_sizes.sh` is non-negotiable. Tighten, never relax.

### 4.4 Performance budget

- Each phase has a perf budget vs. the start of that phase: **±5% on LMbench geomean, ±10% on macro benches** (nginx, redis, sqlite). Regressions outside the budget block the phase.
- Phase 3 may *improve* perf (cooperative switching is cheaper than preemptive). Track separately.

### 4.5 PR conventions

- Subject: `<area>: <imperative>`, e.g., `ostd/mm: introduce Frame<M> with typed metadata`.
- Body must include:
  - Subtask ID(s) closed (`Closes 1B.3, 1B.4`).
  - **TCB delta**: lines of `unsafe` added/removed in `slopos-ostd`.
  - **Test artifact**: `just test` summary line.
  - **Perf delta** for Phase 2+ PRs that touch hot paths.
- Run `cargo fmt --all` before committing (per `CLAUDE.md`).

### 4.6 Soundness invariants reference

These ten invariants come from Asterinas paper §4.3. Every OSTD `unsafe` block must reference at least one.

| Inv. | Statement |
|---|---|
| 1 | A newly-allocated `Frame`/`Segment` originates from currently unused memory. |
| 2 | Kernel-mode CPU state cannot be tampered with by OSTD clients (sensitive RFLAGS bits hidden). |
| 3 | Kernel-mode CPU state cannot be tampered with by peripherals (IOMMU interrupt remapping). |
| 4 | Sensitive memory cannot be tampered with by OSTD clients (only insensitive frames exposed). |
| 5 | Sensitive memory cannot be tampered with by user programs (`VmSpace` only accepts `UFrame`/`USegment` + stack guard pages + 2 KiB frame ceiling). |
| 6 | Sensitive memory cannot be tampered with by peripherals (IOMMU default-deny). |
| 7 | Sensitive I/O memory or ports cannot be tampered with by OSTD clients (`IoMem`/`IoPort` only over insensitive ranges). |
| 8 | A `Task` runs on at most one CPU at any given time. |
| 9 | A `HeapSlot` or any object derived from it must not outlive its parent `Slab`. |
| 10 | An object is created from a `HeapSlot` only if the slot meets the object's size and alignment requirements. |

---

## 5. Phase 1 — OSTD Foundation

> **Status**: ✅ complete — tagged `framekernel-phase-1`.
> **Goal**: a single crate, `slopos-ostd`, contains every line of `unsafe`; all other kernel crates compile under `#![forbid(unsafe_code)]`.
> **Outcome**: behaviorally identical kernel, architecturally a framekernel. TCB ratio 0.678 % (target ≤1.5 %); `just test` 2427/2427.

The migration lifted every `unsafe` block — context switch, FPU XSAVE/XRSTOR, IDT/IRET recovery, user-copy asm, the karch CPU HAL, per-process paging — into `slopos-ostd` behind typed safe APIs (`Frame<M>`, `UFrame`, `VmSpace`/cursor, `IoMem`, `IoPort`, `IrqLine`, `UserContext`, sync + task primitives), folded the global allocator into OSTD, and rewrote every consumer crate to call OSTD. Sub-phases 1A–1J cover that consolidation; what follows closed the phase.

- [x] **A** slibc/userland test-shim layer (`slopos_slibc::alloc::RawBuffer` + per-module `shim.rs`); ~63 test-site unsafes removed. Userland-side, outside the kernel `#![forbid]` set.
- [x] **B** KernMiri port: stock cargo-miri + `cfg(target_os = "none")` host fallbacks run the OSTD algorithms-of-record under Miri (`just check-miri` ≈395 pass / 28 ignored). No fork. B.9 findings-doc skipped per direction; UB fixed inline. See `tools/kernmiri/README.md`.
- [x] **C** Build gates made load-bearing: `check_unsafe_outside_ostd.sh`, `check_alloc_dep.sh`, `check_stack_sizes.sh` (2 KiB / Inv. 5'), `tcb_ratio.sh`, composite `just check-framekernel` (CI-wired). Every Inv. 1–10 named in a `// SAFETY:` comment. Tagged `framekernel-phase-1`.
- [ ] **C.10** Skipped — no pre-Phase-1 LMbench baseline was ever recorded (§ 2J perf verification later dropped).

### Phase 1 Exit Criteria

1. `slopos-ostd` is the only kernel crate with `unsafe`. CI-enforced. ✅
2. TCB ratio ≤1.5 %. ✅
3. KernMiri on `slopos_ostd::mm` / `sync`, zero UB. ✅ (coverage reported, not gated)
4. `just test` at parity. ✅
5. LMbench within ±5 %. — no baseline (C.10 skipped)
6. All ten invariants named in OSTD `// SAFETY:` comments. ✅

---

## 6. Phase 2 — Safe-Rust Kernel Services

> **Status**: ✅ complete — `framekernel-phase-2` close commit pending (2K.6).
> **Goal**: rip-and-replace `mm/`, `core/`, `fs/`, `drivers/`, `net/` as safe-Rust services on OSTD; page allocator, slab, scheduler, syscall dispatch become injectable trait impls *outside* the TCB.
> **Outcome**: `#![forbid(unsafe_code)]` on all 17 non-OSTD kernel crates. TCB ratio 0.722 % (target ≤1 %); `just test` 2458/2458.

Driving rule: anything that can be a safe-Rust trait impl outside OSTD *should* be (Linux's scheduler grew 1.6K→27K LoC inside the TCB; we keep it out by construction). Each subphase registers its impl with OSTD through a one-shot `&BspToken`-gated hook (`register_frame_allocator` / `register_kernel_slab_handle` / `register_scheduler` / `register_kernel_thread_spawner`), all mirroring the same pattern.

- [x] **2A** Page allocator → `BuddyAllocator` (`mm/src/page_alloc/`) impl `FrameAlloc`, BSS singleton registered to OSTD; per-CPU caches via `CpuLocal`. Legacy frame-alloc shim deleted.
- [x] **2B** Slab → `SlabAllocator<const SIZE>` per class (16…2048) + large tier (`mm/src/slab/`) impl `Slab` + new OSTD `KernelHeapBackend`; fn-pointer backend retired, per-CPU magazines re-enabled, heap-VA region retired (slab pages in HHDM).
- [x] **2C** Scheduler → new `sched/` crate; `PriorityScheduler` / `PriorityRunQueue` impl OSTD traits (placeholder dispatch — the preemptive path stays live until Phase 3). `RoundRobinScheduler` deleted; idle task via `register_idle_task_factory`.
- [x] **2D** Typed-argument syscall dispatch: `SyscallArg` / `SyscallArgList` traits, `define_syscall!` macro, 115 handlers migrated; `SyscallContext` no longer exposes raw register accessors.
- [x] **2E** Driver cleanup: `slopos_ostd::task::spawn` safe spawn surface; softirq/bottom-half + C-ABI `spawn_kernel_task` deleted; PCI registry → link-section `.driver_registry` + `pci_driver!` macro.
- [x] **2F** ext2 page cache retyped to `Frame<PageCacheMeta>` (dirty bit + `owner_key` in frame meta); `fs/` audit clean.
- [x] **2G** Net packet pool retyped to `Frame<PacketMeta>` owned by value (`SpinLock` free-stack, was BSS `UnsafeCell` + Treiber stack); `net/` audit clean.
- [x] **2H** Generation-counter handles (AD-11): pure-safe-Rust `Handle<T>` / `HandleTable<T>` in `slopos-ostd/src/handle.rs`. FD/open-file + pipe tables retyped; process-VM table and scheduler task pool keep their pinned storage with a generation overlay (`process_vm_handle`, `task_handle`). Stale refs → typed `HandleError`, never UB.
- [x] **2I** Audit: `#![forbid(unsafe_code)]` on all 17 non-OSTD kernel crates; `check_unsafe_outside_ostd: OK`; TCB ratio 0.722 %.
- [x] **2J** Dropped by direction — no pre-Phase-1 perf baseline existed to gate against. The §4.4 ±5 %/±10 % budget stands as aspiration; per-subphase "perf within ±5 %" checks landed against the `just test` slow-test list.
- [x] **2K** Close: `just check-framekernel` six gates green, `just test` 2458/2458, TCB ratio ≤1 %, `CVSS.md` trimmed to open findings (SLOPOS-2026-0006 ext2 OOB-slice DoS the sole open entry). Status → `phase-3-ready`.
- [ ] **2K.6** Pending — user-performed close commit + `framekernel-phase-2` tag.

### Phase 2 Exit Criteria

1. Every non-OSTD kernel crate carries `#![forbid(unsafe_code)]`. ✅
2. TCB ratio ≤1 %. ✅ (0.722 %)
3. Scheduler, page allocator, slab, syscall dispatch live in non-OSTD crates as trait impls. ✅
4. Generation-counter handles for fds, pipes, page tables, tasks. ✅
5. `CVSS.md` reflects post-Phase-2 attack surface. ✅

---

## 7. Phase 3 — Async-First Task Model

> **Goal**: tasks are `Pin<Box<dyn Future>>`. Cooperative executor primary; preemptive backstop for blocking syscalls and runaway compute. Every blocking syscall handler is `async fn`.
> **Duration estimate**: 10–14 weeks.
> **Depends on**: Phase 2 complete.
> **Phase ends with**: SlopOS is the first production-bound async-first Rust kernel.
> **OSTD itself stays sync** (per AD-9). Async lives in services on top.

### Phase 3 Background

This is the differentiator. No production Rust kernel today is async-first; Theseus dabbled, Drone-OS is the closest production attempt. The pitch: cooperative scheduling is dramatically cheaper than preemptive when it works (no register save, no kernel stack handoff), and Rust's `Future`/`Pin` types are a more natural fit for kernel state machines (waiting on I/O, locks, IPI completions) than blocking-thread primitives.

Hard problems we have to solve:
1. **IRQ handlers stay sync** (no allocation, bounded time). They post completions to async tasks via `WaitQueue`/`Notify`.
2. **TLB shootdowns** — the canonical "sync IPI in the middle of an async syscall" case. Phase 3 designs an async TLB shootdown primitive.
3. **Preemptive backstop** — long-running futures that don't yield are pre-empted at safe points (timer tick + `await` boundary check).
4. **Async syscall completion** — user task is suspended on a future; the executor resumes it when the future is ready, and *then* returns to user mode.

### 3A: Task as Future

- [ ] **3A.1** Define `sched/src/async_runtime.rs`:
  ```rust
  pub struct AsyncTask {
      future: Pin<Box<dyn Future<Output = TaskExit> + Send>>,
      task: slopos_ostd::task::Task,  // OSTD-owned bare task
      state: AsyncTaskState,
      waker: Waker,
  }
  pub enum AsyncTaskState { Ready, Running, Pending, Done(TaskExit) }
  pub enum TaskExit { Normal(i32), Killed(Signal), Panic(KString) }
  ```
- [ ] **3A.2** `pub fn spawn<F: Future<Output = TaskExit> + Send + 'static>(f: F) -> AsyncTaskHandle`. Spawns a kernel-side async task.
- [ ] **3A.3** Implement a `Waker` impl that posts the task back to the scheduler's runqueue. Standard `RawWakerVTable` boilerplate.
- [ ] **3A.4** OSTD's `Task` (1I.4) is unchanged — it's the *bare* underlying primitive. `AsyncTask` is a Phase-2-services concept layered on top (AD-9).

### 3B: Cooperative executor + preemptive backstop

- [ ] **3B.1** `sched/src/executor.rs`: `pub struct Executor { /* per-CPU runqueue of AsyncTasks */ }`.
- [ ] **3B.2** Executor main loop: dequeue ready task, poll its future, if `Pending` move to wait set, if `Ready` re-enqueue, if `Done` deallocate. Per-CPU.
- [ ] **3B.3** Preemptive backstop: timer tick checks `current_task.runtime > QUANTUM`. If so, set a yield flag. The executor checks the flag at every `await` boundary (transparent to user code) and yields if set.
- [ ] **3B.4** For futures that don't `.await` (CPU-bound), the timer tick triggers a forced yield by injecting a software interrupt that the executor catches as a `YieldRequest`. This is the *only* case that uses preemption; the common case stays cooperative.
- [ ] **3B.5** User-mode preemption is unchanged: a timer tick during user-mode execution returns to the kernel via the standard IRQ path, which then yields the executor.
- [ ] **3B.6** Replace `PriorityScheduler` (from 2C) with the async executor. Keep the priority logic.
- [ ] **3B.7** Verify: context-switch micro-benchmark improves vs. Phase 2 (cooperative is cheaper than preemptive).

### 3C: Async syscall surface

Every syscall handler that *could* block becomes `async fn`.

- [ ] **3C.1** Update the `define_syscall!` macro (from 2D.3) to support `async fn` handlers:
  ```rust
  define_syscall!(async read(fd: Fd, buf: UserSlice<u8>, len: usize) -> isize {
      let pipe = pipe_table.get(fd)?;
      let n = pipe.read(buf).await?;
      Ok(n as isize)
  });
  ```
- [ ] **3C.2** Syscall dispatch: when handler is async, the dispatcher polls the future. If `Ready`, return to user mode immediately. If `Pending`, suspend the user task; resume when the future wakes.
- [ ] **3C.3** Migrate every blocking syscall: `read`, `write`, `recvmsg`, `sendmsg`, `ppoll`, `futex_wait`, `nanosleep`, `wait4`, `accept`, `connect`, `pause`, `sigsuspend`, blocking `ioctl`s. Each is one 3C.3.{a..z}.
- [ ] **3C.4** Non-blocking syscalls (`getpid`, `getuid`, `brk`, `mmap` of anonymous memory, etc.) stay sync. The macro accepts both forms.

### 3D: Async sync primitives

Replace the OSTD `WaitQueue` (which is sync) with async equivalents living in services.

- [ ] **3D.1** `sched/src/async_primitives/wait_queue.rs`: `pub struct AsyncWaitQueue { ... }`, `pub fn wait(&self) -> WaitFuture<'_>`. Wakes via stored `Waker`s.
- [ ] **3D.2** `AsyncMutex<T>` — `lock(&self) -> LockFuture<'_>`. Internally a queue of `Waker`s.
- [ ] **3D.3** `AsyncRwLock<T>` — same idea, two queues.
- [ ] **3D.4** `AsyncChannel<T>` — bounded MPSC for kernel inter-task messaging.
- [ ] **3D.5** `Notify` — a single-shot wake signal. IRQ handlers post to `Notify`s; async tasks await them.
- [ ] **3D.6** Convert today's pipe wait-queue logic (per agent memory: `core/src/syscall/tests.rs` pipe blocking path) to `AsyncWaitQueue`.

### 3E: Async TLB shootdown

The hardest single primitive in Phase 3. Today (`mm/src/tlb.rs`) sends sync IPIs and spin-waits for ACK.

- [ ] **3E.1** Design doc as `plans/ASYNC_TLB_SHOOTDOWN.md` before any code. Cover: how the issuing CPU yields while waiting for ACKs; how late-arriving ACKs are handled; how cross-CPU ordering is preserved.
- [ ] **3E.2** `mm/src/tlb.rs::shootdown(...)` becomes `async fn shootdown(...)`. Returns a future that completes when all targeted CPUs have ACKed.
- [ ] **3E.3** IPI handler on receiving CPU stays sync (it must — it's in IRQ context). On completion, it posts to a per-CPU `Notify`. The issuing CPU's future awaits all the `Notify`s.
- [ ] **3E.4** Issuing-CPU yield: the future yields to the executor while awaiting ACKs, freeing the CPU to run other tasks. Today, the issuing CPU spin-waits — wasted cycles.
- [ ] **3E.5** Verify with stress test: 100 concurrent munmaps across 4 CPUs. Check that throughput improves vs. Phase 2.

### 3F: Pipe / poll / futex / signal rewrite

Take advantage of async to clean up code that's currently a manual state machine.

- [ ] **3F.1** Pipe (from agent memory: `fs/src/fileio.rs` per-pipe wait queues): becomes `AsyncWaitQueue` directly. Block-on-empty-read = `wait_queue.wait().await`. Removes the `SendTaskHandle` newtype trick (no longer needed; futures are naturally Send).
- [ ] **3F.2** `ppoll` (per agent memory, syscall 112): becomes a future that races multiple FD readiness futures. Use a `select_all`-style combinator.
- [ ] **3F.3** `futex_wait`: `async fn` that suspends on the futex's `AsyncWaitQueue`. `futex_wake` posts to the queue. Existing semantics preserved.
- [ ] **3F.4** Signal delivery: today's blocking syscalls poll `signal_pending`. Now signals wake the awaiting future via the executor.

### 3G: Performance verification

- [ ] **3G.1** Build and run a perf suite (process create/exec, page fault, mmap, pipe BW, TCP loopback latency — Asterinas paper Table 7 categories). Phase 2 § 2J, which would have built this runner, was dropped, so Phase 3 stands it up fresh.
- [ ] **3G.2** **Expected wins**: context-switch latency, pipe BW (less stack handoff), `ppoll` (no per-FD spin), TLB shootdown (issuing CPU doesn't waste cycles).
- [ ] **3G.3** **Expected costs**: future polling overhead (small but real); compare cooperative-yield path against direct return.
- [ ] **3G.4** Geomean target: improved over Phase 2 by ≥5%, no regressions >10% on any single bench.
- [ ] **3G.5** Document in `plans/PHASE3_PERF_REPORT.md`.

### 3H: Paper draft

Force the design to be coherent by writing it up.

- [ ] **3H.1** Draft `papers/async-first-framekernel.md` (≤4000 words). Sections: motivation, framekernel discipline, async-first task model, TLB shootdown case study, performance.
- [ ] **3H.2** Internal review. The point isn't to publish (yet); the point is that any design that can't be explained in a paper is probably wrong.

### 3I: Phase 3 close

- [ ] **3I.1** All blocking syscalls are `async fn`.
- [ ] **3I.2** OSTD remains sync (`rg async slopos-ostd/` returns zero matches except in docs).
- [ ] **3I.3** Perf within 3G.4 targets.
- [ ] **3I.4** Paper draft exists.
- [ ] **3I.5** `just check-framekernel` zero failures; `just test` full pass.
- [ ] **3I.6** Tag commit `framekernel-phase-3`. Phase-3 close PR.
- [ ] **3I.7** Status → `phase-4-ready`.

### Phase 3 Exit Criteria

1. Every blocking kernel syscall is `async fn`.
2. OSTD itself contains no async (verified by grep).
3. Async TLB shootdown working under stress test.
4. Cooperative scheduling is the primary path; preemption is documented as a backstop.
5. Performance ≥5% geomean improvement over Phase 2.
6. Paper draft exists and is internally coherent.

---

## 8. Phase 4 — Verus Verification of OSTD Critical Path

> **Goal**: machine-checked proofs of the load-bearing OSTD invariants. Not whole-OSTD verification — the *critical path* only.
> **Duration estimate**: 8–12 weeks (2 person-quarters).
> **Depends on**: Phase 3 complete (proofs invalidated by async refactor; do them last).
> **Phase ends with**: SlopOS has a credible "small, partially formally proven, async-first" TCB story.

### Phase 4 Background

Asterinas is heading here with vostd; CortenMM (SOSP '25 Best Paper) is the precedent for verified concurrent paging. We do *not* attempt whole-OSTD verification — that's seL4-decade-of-effort territory. We pick three invariants whose machine-checked proofs maximally strengthen the soundness story.

### 4A: Verus toolchain pinning

- [ ] **4A.1** Choose a Verus commit (latest stable as of phase start). Pin in a new file `verification/verus.toml` with the SHA.
- [ ] **4A.2** Add `verification/` directory: README, `Cargo.toml` for verification crate, `proofs/` subdirectory.
- [ ] **4A.3** Document upgrade procedure: when do we bump Verus? Default: once per quarter, only when a needed feature lands.
- [ ] **4A.4** Add `just verify` recipe that runs Verus over every file in `verification/proofs/`.
- [ ] **4A.5** CI: `just verify` runs on every PR that touches `slopos-ostd/`. Proof regressions block merge.

### 4B: `Frame<M>` ref-count proof

The Asterinas paper found a real UB here via KernMiri (Figure 9). Verus prove it can't recur.

- [ ] **4B.1** Write `verification/proofs/frame_refcount.rs`: a Verus-annotated copy of `slopos_ostd::mm::frame::{Frame, MetaSlot, Drop}`.
- [ ] **4B.2** State invariants (Verus `requires` / `ensures` / `invariant`):
  - "If `frame.ref_count() > 0`, then the underlying physical frame is allocated."
  - "Drop decrements ref count; on transition to 0, releases the physical frame exactly once."
  - "Concurrent `Frame::clone` and `Frame::drop` cannot produce a use-after-free."
- [ ] **4B.3** Prove with Verus. Iterate on annotations until SMT closes the proof.
- [ ] **4B.4** Replace the `slopos-ostd` source with the Verus-annotated version (Verus generates a non-Verus build for the kernel).
- [ ] **4B.5** Verify: `just verify` passes; `just build` and `just test` pass with the verified version.

### 4C: Slab / `HeapSlot` lifetime proof

Inv. 9 + Inv. 10.

- [ ] **4C.1** Write `verification/proofs/slab_lifetime.rs` for `slopos_ostd::mm::heap::{Slab, HeapSlot}`.
- [ ] **4C.2** Invariants:
  - "Inv. 9: `HeapSlot` cannot outlive its parent `Slab`."
  - "Inv. 10: `HeapSlot::into_box::<T>(val)` succeeds only if `slot.size >= size_of::<T>()` and `slot.align >= align_of::<T>()`."
- [ ] **4C.3** Prove. Replace source. Verify build + tests.

### 4D: `VmSpace::cursor` proof

The hardest of the three. CortenMM's open-source proofs are the prior art; lean on them.

- [ ] **4D.1** Read CortenMM paper + open-source proofs (`http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf`).
- [ ] **4D.2** Adapt CortenMM-style invariants to `slopos-ostd::mm::vm_space::Cursor`:
  - "Cursor operations preserve page-table well-formedness (no dangling intermediate frames; all entries point to valid PTEs)."
  - "Concurrent cursors in non-overlapping ranges do not interfere (transactional)."
  - "Mapping a `UFrame` increments its ref count; unmapping decrements."
- [ ] **4D.3** Prove. The hard part is concurrent cursors — may require RCU-style proof obligations.
- [ ] **4D.4** Verify build + tests.

### 4E: CI integration

- [ ] **4E.1** GitHub Actions (or whatever CI we use) runs `just verify` on every PR touching `slopos-ostd/`.
- [ ] **4E.2** Verus output published as a CI artifact (HTML report).
- [ ] **4E.3** Status badge in `README.md`: "OSTD critical path: 3/3 proofs check on Verus <SHA>".

### 4F: Public proof status

- [ ] **4F.1** `verification/STATUS.md`: which OSTD modules are *verified*, which are *audited only*, which are *unaudited*.
- [ ] **4F.2** For each verified module, link the proof file and the spec.
- [ ] **4F.3** Public claim: SlopOS has the smallest formally-verified TCB of any production-bound async-first Rust kernel. Defensible — no other kernel meets all three adjectives.

### 4G: Phase 4 close

- [ ] **4G.1** Three proofs check on the pinned Verus commit.
- [ ] **4G.2** Total proof effort log ≤2 person-quarters.
- [ ] **4G.3** `just check-framekernel` + `just verify` zero failures. `just test` passes.
- [ ] **4G.4** Tag commit `framekernel-phase-4`. Phase-4 close PR.
- [ ] **4G.5** Status → `complete`.

### Phase 4 Exit Criteria

1. Three Verus proofs check on the pinned commit: `Frame<M>` ref-count, slab/`HeapSlot` lifetimes, `VmSpace::cursor` invariants.
2. CI gate on `just verify` for any PR touching OSTD.
3. Public proof status page.
4. Total proof effort ≤2 person-quarters.

---

## 9. Out of Scope / Deferred

These are deliberately *not* in the four-phase plan. Listed here so future agents know they were considered.

| Item | Why deferred |
|---|---|
| **CHERI/Morello pointer tagging** | Hardware availability too thin in 2026. Design OSTD pointer types so they *can* carry tags later (newtype around `*mut T`, never a bare pointer). Revisit 2027–2028. |
| **ARM MTE integration** | Cheaper than CHERI; can be added in a Phase 5 once x86_64 is solid. |
| **ARM64 / RISC-V port** | Single arch keeps OSTD surface tractable through verification. Phase 5+. |
| **User-visible capability system** (seL4-style) | Smaller research delta than async-first; doable inside the framekernel later. |
| **Live evolution** (Theseus-style) | Hard, narrow real-world value. Skip unless a customer demands it. |
| **Multikernel / per-core OS replicas** (Barrelfish) | Adds enormous complexity for unclear win. |
| **Whole-OSTD Verus verification** | Decade of effort. Critical-path verification (Phase 4) gets 80% of the credibility for 2% of the cost. |
| **Formal proof of the Verus toolchain itself** | Out of any sane scope. |
| **TEE / SGX / TDX integration** | Asterinas-flavored use case; can layer on top later. |

---

## 10. Risk Register

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | `VmSpace::cursor` API design wrong on first try; have to rewrite mid-Phase-1 | Medium | High | 1D builds a throwaway prototype before final API. Budget 2 weeks for this. |
| R2 | KernMiri shims drift from real hardware behavior; UB findings false-positive | Low | Medium | 1K.2 chooses simpler stock-Miri-with-shims path; review shims against real hardware annually. |
| R3 | Async TLB shootdown design fundamentally infeasible | Medium | High | 3E.1 mandates a design doc *before* code. If doc fails review, fall back to sync TLB shootdown (lose differentiator partial credit but don't block phase). |
| R4 | Verus pinned fork rots faster than maintained; can't upgrade Rust | Medium | Medium | 4A.3 budgets quarterly Verus bumps. If Verus stops shipping, fall back to Kani for bounded checking. |
| R5 | Performance regression budget blown in Phase 2 | Medium | High | 4.4 sets explicit ±10% bound. If exceeded, profile, fix; don't carry the regression forward. |
| R6 | Async refactor in Phase 3 breaks too much; can't reach parity | Medium | Critical | 3 has "fall back to sync per primitive" escape hatches throughout. Pipes, poll, futex are independent — async-ify them one at a time. |
| R7 | Generation-counter handles add latency to hot paths (FD lookup) | Low | Medium | 2H ships with benchmarks. If lookup latency >2× current, redesign as inline atomic compare. |
| R8 | OSTD module structure (1A.2) needs reshuffling mid-Phase-1 | Low | Low | Cheap to refactor early. Don't be afraid to rename. |
| R9 | Forbid-unsafe gate (1L.1) catches false positives in cfg-gated code | Medium | Low | 1L.1 has cfg-aware lookback (already in `check_alloc_dep.sh`); copy that pattern. |
| R10 | Phase 1 takes >10 weeks; team morale | Low | Medium | Subtask granularity is intentional — every checked box is visible progress. |

---

## 11. References

### Primary papers

- Peng et al., *Asterinas: A Linux ABI-Compatible, Rust-Based Framekernel OS with a Small and Sound TCB*, USENIX ATC '25 — arXiv:2506.03876, https://arxiv.org/abs/2506.03876
- Peng et al., *Framekernel: A Safe and Efficient Kernel Architecture via Rust-based Intra-kernel Privilege Separation*, APSys '24 — https://dl.acm.org/doi/10.1145/3678015.3680492
- Boos et al., *Theseus: an Experiment in Operating System Structure and State Management*, OSDI '20 — https://www.usenix.org/system/files/osdi20-boos.pdf
- Narayanan et al., *RedLeaf: Isolation and Communication in a Safe Operating System*, OSDI '20 — https://www.usenix.org/system/files/osdi20-narayanan_vikram.pdf
- Levy et al., *Multiprogramming a 64KB Computer Safely and Efficiently*, SOSP '17 (Tock) — https://dl.acm.org/doi/10.1145/3132747.3132786
- Klein et al., *seL4: Formal Verification of an OS Kernel*, SOSP '09 — https://read.seas.harvard.edu/~kohler/class/cs260r-17/klein10sel4.pdf
- *CortenMM: Efficient Memory Management with Strong Correctness Guarantees*, SOSP '25 Best Paper — http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf

### Documentation

- The Asterinas Book — https://asterinas.github.io/book/
- The Asterinas Book — Framekernel Architecture — https://asterinas.github.io/book/kernel/the-framekernel-architecture.html
- The Asterinas Book — OSTD Overview — https://asterinas.github.io/book/ostd/
- The Asterinas Book — Writing a Kernel in 100 Lines of Safe Rust — https://asterinas.github.io/book/ostd/a-100-line-kernel.html
- Asterinas blog — Kernel Memory Safety: Mission Accomplished — https://asterinas.github.io/2025/06/04/kernel-memory-safety-mission-accomplished.html
- Hubris reference (Oxide) — https://hubris.oxide.computer/reference/
- Verus guide — https://verus-lang.github.io/verus/guide/

### Source repositories

- Asterinas — https://github.com/asterinas/asterinas
- vostd (Verus verification of OSTD) — https://github.com/asterinas/vostd
- OSTD on crates.io — https://docs.rs/ostd/
- Theseus — https://github.com/theseus-os/Theseus
- Hubris — https://github.com/oxidecomputer/hubris
- Tock — https://github.com/tock/tock
- Verus — https://github.com/verus-lang/verus

### Internal SlopOS context

- This plan's research synthesis: previous chat in this session.
- `CLAUDE.md` — repository conventions.
- `AGENTS.md` — agent guidelines.
- `plans/LEGACY_MODERNIZATION_PLAN.md` — adjacent plan; Phases 0–5 already done. Reference for plan-document style.
- `plans/ANALYSIS_SLOPOS_VS_LINUX_REDOX.md` — competitive baseline.
- Audit of current SlopOS unsafe surface (research summary in chat history).
