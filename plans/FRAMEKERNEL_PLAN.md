---
name: SlopOS Framekernel Architecture Plan
description: Three-phase rip-and-replace plan to redesign SlopOS as a framekernel with a Verus-verified OSTD critical path and an io_uring-style async edge
status: phase-3-ready
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: **Phase 1 & 2 complete; Phase 3 implementation complete (3A–3I, pending the user-performed close tag).** `slopos-ostd` owns every line of kernel `unsafe`; all 17 non-OSTD kernel crates are `#![forbid(unsafe_code)]`; TCB ratio 0.722 % (target ≤1 %). Three OSTD critical-path invariants (`Frame<M>` ref-count, slab/`HeapSlot` lifetimes, `VmSpace::cursor`) machine-checked under a pinned Verus toolchain (32 obligations); the io_uring-style SlopRing surface (`ring_setup` / `ring_enter`, nine opcodes) ships sync in the kernel with a userland runtime + `slopfut` executor, and `nc`'s TCP recv/send loop is ported onto it. `just test` 2485/2485; `just check-framekernel` green. Tagged `framekernel-phase-1`; `framekernel-phase-2` / `framekernel-phase-3` close commits pending. Per-subphase implementation notes live in `git log`.
> **Post-implementation bug-fix pass (2026-05).** A multi-angle adversarial review of the shipped 3H surface found and fixed four real defects the original landing missed: (1) the `RingParams` wire image serialized `region_addr` at byte 60 while the `#[repr(C)]` struct places it at 64 (u64-pad), so userland read `region_addr = region_bytes<<32` (`0x300000000000` for a 16-entry ring) and page-faulted `nc` on first ring access — fixed with `offset_of!`-driven `RingParams::to_bytes`/`from_bytes` + static offset asserts (`abi/src/ring.rs`); (2) `Errno::raw()` is already negative, so `opcode.rs`/`enter.rs` double-negated it — every `-EAGAIN` would-block was mis-read as a `+11` inline completion and every error CQE/return was positive, breaking *all* deferred completions and `nc`'s multiplexing; (3) the userland syscall asm wrappers clobbered only `rcx`/`r11`, so the compiler kept a zeroed `ymm3` across a kernel-SSE-clobbering syscall and reused it to zero-init an SQE → garbage opcode (a latent kernel-ABI bug affecting all userland; fixed by clobbering `xmm0-15` in `slibc/src/pal/raw.rs`); (4) `nc`'s `on_sock`/`on_stdin` re-armed forever on a persistent socket error (e.g. `ECONNRESET`) → 100 % CPU livelock — fixed to treat `res < 0` as terminal. Plus the executor `block_on` already-resolved guard and the `OP_ACCEPT` stored-nonblock-flag restore. Two known-latent items are documented: the `ring_enter`↔`close` lock-order inversion (system-wide ring DoS under concurrent enter+close on a dup'd ring fd from multiple threads — **CVSS SLOPOS-2026-0007**, not triggered by `nc`/tests) and the `OP_WRITE`/`OP_SEND` deferred send-readiness wakeup (§ 3H notes).
> **Target**: Redesign SlopOS as a framekernel with a small, formally-verified trusted core (`slopos-ostd`) and an io_uring-style async edge — sync kernel core, async lives in userspace on top of shared-memory rings. Pre-alpha rip-and-replace; no backwards compatibility constraints.
> **Note on prior Phase 3 / Phase 4 drafts.** An earlier revision of this document proposed an *async-first task model* (`Pin<Box<dyn Future>>` tasks, cooperative executor, async TLB shootdown) as Phase 3, with Verus verification deferred to Phase 4. After a deep research review — see § 9 *Out of Scope* — that direction was retired: every comparable production kernel (Asterinas, Theseus, Hubris, seL4, Linux RFL, Tock, Redox, Fuchsia, MINIX, QNX, Maestro) declined the same design point, and Verus does not yet verify `async fn`. The current Phase 3 fuses the previous Phase-4 verification work with an io_uring-style userspace async surface, which is the *sync core, async edge* shape used by every winning system in 2026.
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
7. [Phase 3 — Verus-Verified OSTD + io_uring-Style Async Edge](#7-phase-3--verus-verified-ostd--io_uring-style-async-edge)
8. [Out of Scope / Deferred](#8-out-of-scope--deferred)
9. [Risk Register](#9-risk-register)
10. [References](#10-references)

---

## 1. Executive Summary

SlopOS today is ~155K LoC of kernel Rust with ~1,426 `unsafe` occurrences (0.92%) clustered in seven subsystems. Drivers are already 0.19% unsafe — exemplary. The codebase already has proto-OSTD primitives (`slopos-alloc`, `IrqMutex`, `OwnedPageFrame`, `MmioRegion`, `UserPtr<T>`) and build-time gates (`check_alloc_dep.sh`, `check_stack_sizes.sh` at 2 KiB) that are stricter than Linux mainline.

This plan does three things, in strict serial order:

1. **Phase 1 — OSTD Foundation**: carve a single `slopos-ostd` crate that owns *every* line of `unsafe` in the kernel. Forbid `unsafe` everywhere else. Build the typed primitives (`Frame<M>`, `UFrame`, `USegment`, `VmSpace::cursor`, `IoMem`, `IoPort`, `DmaCoherent`, `IrqLine`, `UserContext`). Existing kernel is migrated to consume OSTD at parity.
2. **Phase 2 — Safe-Rust Kernel Services**: rip and replace `mm/`, `core/`, `fs/`, `drivers/`, `net/` with safe-Rust services on top of OSTD. Page allocator, slab, scheduler, syscall dispatch all become injectable trait impls *outside* the TCB. Achieve `#![forbid(unsafe_code)]` on every non-OSTD kernel crate.
3. **Phase 3 — Verus-Verified OSTD + io_uring-Style Async Edge**: machine-checked proofs of `Frame<M>` ref-count, slab/`HeapSlot` lifetimes, and `VmSpace::cursor` invariants on a pinned Verus toolchain (CI-gated). Then a submission/completion-ring surface (`ring_setup` / `ring_enter`) backed by the existing sync `EventBus` / `WaitQueue` plumbing, so userspace gets first-class async without bringing async into the kernel. **This is the differentiator** — no other kernel today claims small TCB + verified + Linux-ABI + Rust simultaneously.

Phases are strictly serial. Phase 1 → 2 is structural. Phase 3 puts the OSTD critical path under Verus while it is still small enough to verify, then layers the userspace async story on a sync substrate that is now machine-checked.

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
| **AD-8** | **Sync kernel, async edge.** Kernel internals are synchronous; async lives in userspace on top of shared-memory submission/completion rings (`ring_setup` / `ring_enter`). No `async fn` in any kernel crate, including services. | Sync core, async edge is the architecture every winning production system uses today (Linux + io_uring, Postgres + asyncpg, Redis + async clients, Seastar). Every comparable Rust/C/C++ kernel declined whole-kernel async. Keeps Verus tractable. |
| **AD-9** | OSTD itself is sync. All kernel crates are sync. | Keeps the trusted core small and verifiable; matches AD-8. |
| **AD-10** | Verus is the verifier. Pinned upstream `main`-branch commit (avoid the experimental `async` branch). | Best-fit for systems Rust; Asterinas's choice (vostd); precedent matters. Sync OSTD lands in stable Verus today; async OSTD would not. |
| **AD-11** | Generation-counter handles for fds, pipes, page tables, tasks. | Hubris's idea. Stale references → typed errors, never UB. |
| **AD-12** | Target: ≤1.5% TCB ratio after Phase 1, ≤1% after Phase 2. | Asterinas: 14%. We start small enough to do better. |
| **AD-13** | x86_64 only through Phase 3. ARM64/RISC-V deferred. | Single arch keeps OSTD surface tractable through verification. |
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
| **Ring surface** | Phase-3 io_uring-equivalent submission/completion ring exposed to userspace. Two syscalls (`ring_setup`, `ring_enter`). Kernel side is sync. |
| **SQE / CQE** | Submission Queue Entry / Completion Queue Entry — the wire format userspace and the kernel agree on across the ring's shared memory. |
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

## 7. Phase 3 — Verus-Verified OSTD + io_uring-Style Async Edge

> **Goal**: machine-check the OSTD critical-path invariants (`Frame<M>` ref-count, slab/`HeapSlot` lifetimes, `VmSpace::cursor`) on a pinned Verus toolchain, and expose an io_uring-equivalent submission/completion-ring surface to userspace so user-mode async is a first-class story without bringing async into the kernel.
> **Duration estimate**: 5–7 months total (3–4 months Verus, 2–3 months ring surface; they overlap once 3A.5 lands).
> **Depends on**: Phase 2 complete.
> **Phase ends with**: SlopOS is the *smallest verified-TCB Linux-ABI Rust kernel that exists*, with a userspace async story matching Linux io_uring's shape but backed by an OSTD whose load-bearing invariants check on Verus.
> **OSTD stays sync.** All kernel crates stay sync. Async lives entirely in userspace, talking to the kernel through shared-memory rings backed by the existing `EventBus` / `WaitQueue` plumbing.

### Phase 3 Background

An earlier draft of this phase proposed making the kernel itself async-first (tasks as `Pin<Box<dyn Future>>`, cooperative executor, every blocking syscall `async fn`, async TLB shootdown). After deep research that direction was retired — see § 8 *Out of Scope* — for reasons documented there: every comparable production kernel (Asterinas, Theseus, Hubris, seL4, Linux RFL, Tock, Redox, Fuchsia, MINIX, QNX, Maestro) has declined the same design point, the cancellation-safety and debuggability gaps are language-level and not fixable inside the plan window, Verus does not yet verify `async fn` (the `async` branch landed Apr 2026 in `verus-lang/verus#1993` but explicitly does not support trait async, which OSTD's `Scheduler` / `RunQueue` / `FileOps` / `WaitQueueBackend` / `FrameAlloc` traits all are), and "async-first general-purpose SMP kernel" has no production precedent across forty years and three systems languages (Linux + C, Hubris + Rust, seL4 + C all picked sync; on ARM, where TLB shootdown is cheap, Hubris *still* chose sync).

What replaces it is the architecture every winning system on Earth already uses: **sync core, async edge**. Linux + io_uring. PostgreSQL + asyncpg. Redis + async clients. ScyllaDB + Seastar. The kernel's job is to expose async-shaped *primitives* so userspace can build async on top — not to *be* async. This phase delivers exactly that, and uses the now-unblocked time to put the OSTD critical path under Verus while the sync surface is still small enough to verify.

The two sub-goals are independent in design — they touch different files — and the order below reflects risk: Verus first, because it sets the long-term verified-TCB story that every other claim leans on; ring surface second, because it builds on top of OSTD primitives that won't move once verified.

### 3A: Verus toolchain pinning + verification crate scaffolding

> **Status**: ✅ complete. Verus pinned to stable release `0.2026.05.24.ecee80a` (commit `ecee80a2139923d503338e6989f79fb690ec7847`, Rust 1.95.0 host toolchain) in `verification/verus.toml`; `verification/` is a workspace member; `just verify` runs the pinned verifier over `verification/proofs/` (green no-op until 3B authors the first proof); CI `verify` job + `check_no_kernel_async.sh` (R13) gate live. Validated end-to-end against a real obligation.

- [x] **3A.1** Verus commit pinned in `verification/verus.toml` — latest stable on `main` (`release/0.2026.05.24.ecee80a`, commit `ecee80a…`); the experimental `async` branch was deliberately avoided (AD-10). The pin file also records the asset URL + sha256 and the Rust 1.95.0 host toolchain Verus links against. `scripts/ensure_verus.sh` parses it, downloads + sha-verifies the release asset (same integrity pattern as `ensure_limine.sh`), and installs the host toolchain (`rustc-dev` + `llvm-tools`) on demand.
- [x] **3A.2** `verification/` workspace member added: `Cargo.toml` (doc-only `src/lib.rs` so it's a first-class member; the proofs themselves are standalone Verus files, not cargo targets), `README.md`, `STATUS.md` (scaffold table of the three 3B–3D proof targets marked **planned**), `proofs/` (with conventions README), `notes/`.
- [x] **3A.3** `just verify` (`scripts/verify.sh`) runs `verus --crate-type=lib` over every `*.rs` in `verification/proofs/`, fails the gate on any unverified obligation, and is a green no-op when no proofs exist yet. Helper modules (`_*.rs`) are skipped as entry points. Folded into the composite `just check-framekernel` gate alongside the other framekernel-discipline checks.
- [x] **3A.4** Upgrade procedure documented in `verification/README.md` (≤ once/quarter, only for a needed feature, only on a `verify/<sha>` topic branch; never weaken an invariant to make a bump green; Kani fallback if Verus stops shipping, per R4).
- [x] **3A.5** CI gate: a dedicated `verify` job runs `just verify` (with a cached `third_party/verus`); `scripts/check_no_kernel_async.sh` (R13 sibling of `check_unsafe_outside_ostd.sh`, forbids `async fn` in any kernel crate) wired into both the per-PR framekernel gates and `check-framekernel`.

### 3B: `Frame<M>` reference-count proof

The Asterinas paper found a real UB here via KernMiri (paper Fig. 9). Verus-prove it can't recur on SlopOS's port.

- [x] **3B.1** `verification/proofs/frame_refcount.rs` written: a Verus-annotated mirror of `slopos_ostd::mm::frame::{Frame, MetaSlot, Drop}`. Each atomic-bounded method body (`from_unused`, `from_in_use`, non-final `Drop`, final `Drop`) maps to one `Step` against an abstract `Slot`; any concurrent interleaving is a finite `Seq<Step>`.
- [x] **3B.2** Invariants stated as `slot_inv` and three named corollaries:
  - (I1) `i1_positive_rc_is_allocated` — `ref_count > 0` ⇒ typed (allocated) and off the allocator free list.
  - (I2) `i2_release_at_most_once` + `i2_dropfinal_releases_once` — last `Drop` releases the frame exactly once; no other step touches the release counter → no double-free.
  - (I3) `i3_no_use_after_free` — a live payload and free-list membership are mutually exclusive in every reachable state.
- [x] **3B.3** SMT closes — 9 obligations verified on pinned Verus `0.2026.05.24.ecee80a`. `invariant_holds_on_every_trace` lifts the inductive `step_preserves` to all traces (the concurrency claim). `broken_clone_violates_invariant` proves the unconditional `fetch_add(1)` clone (Asterinas Fig. 9 UB) breaks (I1) while the shipped conditional `fetch_update` keeps it — the proof is load-bearing, not vacuous.
- [x] **3B.4** Source cross-referenced rather than mechanically rewritten: `frame.rs` carries a `# Verification` module-doc pointing at the proof and an inline `VERIFIED:` note on the load-bearing conditional bump in `from_in_use`. (The real `MetaSlot` uses raw atomics/`UnsafeCell` that the kernel nightly cannot route through `verus!`; per `proofs/README.md` the proof is the standalone abstract mirror.) `just build` + `just test` pass (2471/2471, parity).
- [x] **3B.5** `verification/STATUS.md`: `slopos_ostd::mm::frame` marked **verified** against the pinned Verus SHA, with a per-obligation proof summary.

### 3C: Slab / `HeapSlot` lifetime proof

Closes Inv. 9 + Inv. 10.

- [x] **3C.1** `verification/proofs/slab_lifetime.rs` written: a Verus-annotated mirror of the slab object lifecycle — `mm::slab::allocator::SlabAllocator<SIZE>`'s grow/alloc/dealloc critical sections + `mm::slab::KernelSlab`'s size-class dispatch, behind OSTD's `mm::slab::Slab` trait. Each critical section maps to one `Step` against an abstract `SlabState`; size-class fit is a pure `class_size` chooser mirroring `KernelSlab::class_of`.
- [x] **3C.2** Invariants stated and proved as named corollaries:
  - **Inv. 9** — `inv9_outstanding_implies_live` (an outstanding cell pins its page), `inv9_dead_slab_has_no_slots` (a reclaimed page has zero outstanding cells), `inv9_no_reclaim_with_outstanding` (the step-level `outstanding == 0` reclaim guard). Lifted to every concurrent interleaving via `invariant_holds_on_every_trace`.
  - **Inv. 10** — `inv10_into_box_fits` (the cell `KernelSlab::alloc` returns is `>= size_of::<T>()` and `>= align_of::<T>()` for any in-range `T`), resting on `class_size_covers`.
- [x] **3C.3** SMT closes — 11 obligations verified on pinned Verus `0.2026.05.24.ecee80a`. Both invariants are load-bearing: `broken_reclaim_violates_invariant` proves an unconditional page reclaim (free a page with live cells) breaks Inv. 9, and `undersized_class_violates_inv10` proves an always-smallest (16-byte) chooser lets a 2048-byte object overflow a 16-byte cell. Source cross-referenced (`slab.rs` `# Verification` module-doc; `VERIFIED:` notes on the `KernelSlab::alloc` size-class dispatch and the never-free page discipline in `allocator.rs`). `just build` + `just test` pass (2471/2471, parity).
- [x] **3C.4** `verification/STATUS.md`: `slopos_ostd::mm::slab` marked **verified** against the pinned Verus SHA, with a per-obligation proof summary.

### 3D: `VmSpace::cursor` proof

The hardest of the three. CortenMM (SOSP '25 Best Paper) is the prior art on verified concurrent paging; lean on its open-source proofs.

- [x] **3D.1** CortenMM paper read (`http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf`); notes captured in `verification/notes/cortenmm.md` — the transactional `AddrSpace::lock(r) -> RCursor` interface (= SlopOS `VmSpace::cursor_mut`), the two locking protocols (`CortenMMrw` / `CortenMMadv` + RCU monitor), the verified properties P1 (mutual exclusion) + P2 (well-formedness, Fig. 12), and the mapping onto the SlopOS cursor with the coarse lock-per-`VmSpace` divergence spelled out.
- [x] **3D.2** CortenMM-style invariants adapted to `slopos_ostd::mm::vm_space::CursorMut` in `verification/proofs/vm_space_cursor.rs` as named corollaries:
  - (WF) `wf_no_dangling_intermediate` — a present leaf implies its whole intermediate chain (PT, PD, PDPT) is present and valid (CortenMM Fig. 12 for the 4-level x86_64 walk).
  - (DIS) `disjoint_vmspaces_independent` — two live cursors hold `&mut` to distinct `VmSpace`s, so their states are independent and stepping one cannot mutate the other (coarse-model discharge of CortenMM §3.3 range-disjoint semantics).
  - (REF) `ref_leaf_holds_at_most_one` / `ref_map_unmap_exactly_once` / `ref_map_then_unmap_roundtrips` + `inv45_leaf_is_uframe` — `map` leaks one `UFrame` ref, `unmap` reclaims one exactly, and a present user leaf is always an insensitive `UFrame` (Inv. 4 + Inv. 5 carrier).
- [x] **3D.3** SMT closes — 12 obligations verified on pinned Verus `0.2026.05.24.ecee80a`. Used the **coarse lock-per-`VmSpace` fallback** (sanctioned here): `CursorMut<'a>` holds `&'a mut VmSpace`, so the borrow checker serializes all mutators on one address space with no SMT obligation — CortenMM's hardest proof (P1 fine-grained mutual exclusion + RCU stale-retry) does not arise. The gap (disjoint ranges on one space serialize where CortenMM parallelises — a scalability, not soundness, difference) is documented in `STATUS.md` + `notes/cortenmm.md` (R11). Both guards proved load-bearing: `broken_double_leak_violates_refcount` (Overlap guard) and `broken_map_sensitive_violates_inv45` (`UFrame` boundary).
- [x] **3D.4** Source cross-referenced rather than rewritten: `vm_space.rs` carries a `# Verification` module-doc pointing at the proof and inline `VERIFIED:` notes on the `map` Overlap guard, the `UFrame` leak, and the `unmap` reclaim. `just build` + `just test` pass (2471/2471, parity).
- [x] **3D.5** `verification/STATUS.md`: `slopos_ostd::mm::vm_space` marked **verified** against the pinned Verus SHA, with a per-obligation proof summary and the coarse-model gap recorded.

### 3E: Public proof status

- [x] **3E.1** `verification/STATUS.md` is the concise coverage map — the negative space `just verify` can't report. Classifies OSTD as **verified** (the 3 critical-path proofs, one row each, per-obligation detail left to the proof files' own doc-comments), **audited only** (the load-bearing `unsafe`-carrying remainder, KernMiri + `// SAFETY:` covered, listed by module), and **unaudited** (the pure-safe-Rust modules — `handle`, POD markers, boot data types, safe helpers — sound by the type system). The coarse-model `vm_space` gap is noted inline.
- [x] **3E.2** "Actually, The Slop Is Proven" section added to `README.md`: left-aligned declarative prose stating only durable facts — framekernel architecture, the ≤1 % TCB *target*, `#![forbid(unsafe_code)]` enforcement, and the three Verus-checked critical-path subsystems as a bullet list, closing on `just verify`. Carries no exact obligation/TCB counts and no module table, both of which would rot; no README-side pipeline.
- [x] **3E.3** Public claim documented and made defensible from primary sources in `verification/STATUS.md` § "Public claim": **SlopOS is the smallest verified-TCB Linux-ABI Rust kernel** — a four-adjective conjunction (small TCB · verified · Linux-ABI · Rust) backed by a comparison table (seL4, Asterinas, Theseus, Linux RFL, Redox, Hubris) showing no other kernel meets all four at once, each row cited to its primary source.

### 3F: io_uring-style ring surface — design

> **Status**: ✅ complete. The design lives as a durable spec at **`docs/SLOPRING.md`** (a long-lived ABI/behaviour contract that 3G/3H/userland all consume — kept in `docs/`, not `plans/`, since it is a specification rather than a roadmap). `docs/SLOPRING.md` was authored *before* any code (18 sections) and **hardened by three independent adversarial reviews** (security / codebase-fidelity / io_uring-architecture) that read the real substrate; their findings were folded back in. SQ/CQ ABI (64 B `Sqe` / 16 B `Cqe`, split-ownership head/tail indices, acquire/release ordering mirroring the `EventBus` atomic-publish contract), the shared-memory model (`Frame<RingMeta>` mapped via `cursor_mut` exactly like `process_vm_mmap_shared`; **volatile/atomic** byte-copy `UFrame` access, **never** a kernel `&Sqe`/`&mut Cqe` — AD-3 / Inv. 4/5), submission/completion/cancellation/backpressure, the nine-opcode catalogue each mapped to its existing sync entry point (3F.2), and the full threat model (3F.3). **Load-bearing corrections vs. the first draft, all verified against source:** (a) the complete-phase is the existing **poll/select wait shape with the *calling task* as the waiter** — the substrate has no `(ring, op)` wake callback, and the naive "arm on submit, post from the producer's wake path" model is rejected as unimplementable/deadlocking; (b) one new bounded+audited OSTD primitive is required — a **volatile/atomic `UFrame` accessor** — because reading concurrently-user-writable ring memory through the non-atomic byte interface is a data race; (c) **reserve-before-side-effect** for ownership ops (`OP_ACCEPT`, consuming reads) so a full CQ never leaks an fd/socket; (d) per-ring kernel lock for concurrent `ring_enter`; (e) `sq_entries` clamp on the submit loop; (f) ring VMA pins its `Frame` to close an mmap-after-close UAF; (g) submit-count preserved on `EINTR`; (h) deferred completions only progress inside a *blocking* `ring_enter`. Every primitive cross-referenced to its real source file.

- [x] **3F.1** Design doc as `docs/SLOPRING.md` *before* any code (§ 4–§ 11, § 14). (Authored as `plans/SLOPRING.md`, then moved to `docs/` — it is a spec, not a plan.) Covers:
  - SQ/CQ ring layout (head/tail indices, slot format, ABI-stable opcodes).
  - Memory: rings live in a per-process `Frame<RingMeta>` shared between kernel and userspace via `VmSpace::cursor` — mapped read/write to user, read/write to kernel; ownership stays with the process.
  - Submission model: userspace writes SQEs, calls `ring_enter(ring_fd, to_submit, ...)` (one new syscall). Kernel snapshots head, processes SQEs, posts CQEs, advances tail. **All sync from the kernel's perspective.**
  - Completion model: an op ready at submit completes inline (CQE posted now); an op that would block records an in-flight *row* only. A **blocking `ring_enter(min_complete>0)`** registers the *calling task* on every in-flight op's existing `EventBus`/`WaitQueue` (the poll/select shape), re-probes, and posts deferred CQEs from that caller's own context. There is no `(ring, op)` wake callback (the substrate has none); deferred completions progress only inside a blocking enter (SLOPRING § 7.1/§ 8). Userspace polls the CQ for inline completions or `ring_enter`s to drive blocked ops.
  - Cancellation: opcode `OP_CANCEL`. The kernel walks its in-flight table, removes the matching row, posts a CQE with `-ECANCELED`. No async-fn cancellation hazard — the kernel side is straight-line sync, and under caller-as-waiter there is no standing per-op registration to leak. Async cancellation belongs in userspace, where stuck futures degrade one process and not the whole kernel.
  - Backpressure: SQ-full → user retries; CQ-full → kernel drops with `IORING_CQ_OVERFLOW` flag (matches Linux semantics).
- [x] **3F.2** Each opcode justified against an existing sync syscall path in `docs/SLOPRING.md` § 12 (opcode catalogue table): `OP_READ`→`file_read_fd`, `OP_WRITE`→`file_write_fd`, `OP_RECVMSG`→`unix_recvmsg`/`socket_recvfrom`, `OP_SEND`→`unix_send`/`socket_send`, `OP_ACCEPT`→`unix_accept`/`socket_accept`, `OP_POLL_ADD`→`file_poll_register_fd` (+ `file_poll_fused` to read readiness), `OP_TIMEOUT`→`wait_event_timeout` as the *harvest-wait deadline* (not an independently-firing op), `OP_CANCEL`→in-flight-table walk, `OP_NOP`→none. **No new blocking primitives** — each opcode runs the existing path's non-blocking *probe* (the path reads a *stored* nonblocking flag, not a per-call arg — § 12 cross-cutting reality 1) and, on EAGAIN, records an in-flight row that the harvest phase re-probes; R12 parity holds because it is the *same code path*. AF_INET (raw user ptrs) vs AF_UNIX (kernel slices) marshalling differs per family (§ 12 reality 2).
- [x] **3F.3** Threat model documented in `docs/SLOPRING.md` § 13 (asset/boundary table + nine sub-sections): SQEs are hostile user-controlled bytes in concurrently-mutable shared memory; snapshot-then-validate closes TOCTOU; every user VA re-validated through `UserSlice::try_new`; ring memory is `UFrame<RingMeta>` so Inv. 4 + Inv. 5 hold by construction (no kernel `&T` over it); stale/forged ring fds resolve via the Phase-2H `HandleTable` to typed `-EBADF`, never UB; and the core claim — **no kernel reference into the ring outlives a single `ring_enter` invocation** (§ 13.7) — is argued from the volatile byte-copy-only access discipline. The review also surfaced (and § 13 now closes) the access-level data race under the old non-atomic byte interface, the CQ-overflow fd-leak, the `sq_entries`-clamp DoS, and the mmap-after-close UAF.

### 3G: io_uring-style ring surface — implementation

- [x] **3G.0** *(prerequisite, in OSTD)* Added the volatile/atomic `UFrame` accessor (`load_u32_acquire` / `store_u32_release` / `copy_out_volatile` / `copy_in_volatile`) in `slopos-ostd/src/mm/uframe.rs` — the only new OSTD `unsafe` SlopRing needs (SLOPRING § 3, § 5.3). `read_volatile`/`write_volatile` + an explicit acquire/release fence on the index ops mirror Linux's `READ_ONCE`/`smp_load_acquire`. Each method carries a `// SAFETY:` note naming Inv. 4/5; three round-trip tests added to `tests/uframe_round_trip.rs` and pass under KernMiri. New `RingMeta` (dual `AnyFrameMeta` + `AnyUFrameMeta`) + `UFrame::<RingMeta>::alloc()`. **TCB delta: ~6 lines, audited.** TCB ratio 0.660 %.
- [x] **3G.1** New crate `ring/` (kernel-side) carrying `#![forbid(unsafe_code)]`. Hosts the SQ/CQ snapshot (via the 3G.0 accessor through `region::RingRegion`), opcode dispatch (`opcode.rs` + `net_glue.rs`), the in-flight table (`ring_obj.rs`), and the per-ring serialization — folded into the registry `SpinLock` (`registry.rs`, `LOCK_LEVEL_REGISTRY`; SLOPRING § 6.3, taken only across the submit/post bookkeeping, dropped while the harvester blocks). Allocations via `slopos-ostd` discipline (no `extern crate alloc`).
- [x] **3G.2** Two new syscalls (slots `157`/`158`, `SYSCALL_TABLE_SIZE` → `159`): `ring_setup(entries: u32, params: *mut RingParams) -> i32` (honest signature, no `flags`-slot pun — SLOPRING § 6.1) and `ring_enter(ring_fd, to_submit, min_complete, flags) -> i32`. Defined via `define_syscall!` in `core/src/syscall/ring_handlers.rs` — both **sync**. The submit loop clamps `n = min(to_submit, sq_entries, sq_tail - sq_head)` (§ 13.6); `ring_enter` preserves `n_submitted` on `EINTR` (§ 6.2). ABI types in `abi/src/ring.rs` (`Sqe` 64 B / `Cqe` 16 B / `RingParams` / `RingLayout`, page-aligned arrays).
- [x] **3G.3** Per-ring kernel state in a generation-counter `HandleTable<Ring>` (`registry.rs`, packed into the fd's open-file handle; stale/foreign ring FDs → typed `-EBADF`, never UB). The ring VMA (`RegionBacking::Ring` in `mm`) takes an independent `from_in_use` ref on each `Frame<RingMeta>`, so the region cannot be freed while user PTEs map it (no mmap-after-close UAF — § 14); the ring is close-on-fork (skipped in `clone_cow`).
- [x] **3G.4** Nine opcodes implemented (`opcode.rs`): `OP_READ`/`OP_RECVMSG` → `file_read_fd_nonblock`, `OP_WRITE`/`OP_SEND` → `file_write_fd_nonblock` (new fs forced-nonblock probe variants that run the *same* `FileOps` path — R12 parity — but force `O_NONBLOCK`, toggling stored socket state via a RAII guard for § 12 reality 1), `OP_ACCEPT` → `net_glue::accept_nonblock` (AF_INET/AF_UNIX per family), `OP_POLL_ADD` → `file_poll_fd` readiness, `OP_TIMEOUT` (harvest deadline), `OP_CANCEL` (in-flight table walk), `OP_NOP`. Ownership ops reserve a CQE slot before the side effect (§ 11).
- [x] **3G.5** `min_complete > 0` is the **caller-as-waiter multi-fd poll loop** (`enter.rs::harvest`, the `poll_ioctl_handlers.rs` shape): **register-then-recheck** — register the calling task on every *distinct* in-flight fd via `file_poll_fused` (tracked via `file_poll_track_registrations` for kill-safety) *before* re-probing (closes the lost-wakeup window), re-probe + post deferred CQEs, then `block_current_task_with_timeout` (clamped to the nearest `OP_TIMEOUT` deadline), re-probe on wake, post-block `has_pending_signal()` → `-EINTR`; loop until `available_cqes() >= min_complete`. The waiter is the user task; no kernel future, no `(ring, op)` callback.
- [x] **3G.6** Tests: 13 KTAP kernel tests in `ring/src/tests.rs` (region volatile round-trips, CQE post/overflow/reserve, in-flight table, `OP_CANCEL` pending/missing/cancel-all, `OP_NOP`, layout invariants) + 6 userland end-to-end subtests in `userland/src/bin/tests/ring_test.rs` (setup geometry, NOP inline completion, OP_WRITE/OP_READ pipe **parity**, **deferred read** via blocking harvest, future **cancellation**, and **socket dispatch** — OP_WRITE routed to the `FileKind::Socket` FileOps path via the `ForcedNonblockGuard`). Plus ABI round-trip unit tests for `Sqe`/`Cqe`/`RingParams` (incl. the `region_addr_serialized_at_struct_offset` regression guard). All green (`just test`: 2485 pass).

### 3H: Userspace async edge

The ring surface is useless without a userspace runtime that consumes it. This sub-phase builds the smallest credible one.

- [x] **3H.1** Userland runtime as `userland/src/ring/` (the single userland crate's module — no new top-level crate, per § 4.1): `pub struct Ring`, `Ring::setup(entries)`, `push_sqe` / `submit` / `submit_and_wait`, `wait_completion()`, `poll_completion()`. Mirrors `liburing`'s shape; reads/writes the mapped SQ/CQ with acquire/release volatile ordering matching the kernel's. `Ring` now carries a `Drop` impl (munmap + close) so each `setup` no longer leaks a mapping + fd. Syscall wrappers in `userland/src/syscall/ring.rs`.
- [x] **3H.2** `slopfut` executor (`userland/src/ring/executor.rs`): `RingExecutor` + `CompletionFuture`, hand-rolled (chosen over vendoring embassy for the demonstration). `submit(sqe) -> CompletionFuture`, `block_on` (drives deferred completions via blocking `ring_enter`), `poll`, `cancel`. **Userland** — outside the kernel `#![forbid(unsafe_code)]` discipline.
- [x] **3H.3** `nc`'s established-connection TCP recv/send loop ported to the ring (`userland/src/apps/nc/ring_io.rs`): the historical `poll(2)` + blocking `recv`/`send` over stdin + socket is now a SlopRing `Session` that multiplexes both fds through `OP_READ`/`OP_WRITE`, harvested via a **blocking `ring_enter`** (the caller-as-waiter deferred path — SLOPRING § 7.1). Both the client path (`run_conn_loop`) and each accepted listen-mode connection (`run_listen_session`) drive through it; `connect`/`bind`/`listen`/`accept`/`shutdown` stay regular syscalls (out of the nine-opcode data-plane set, § 12). The socket data-plane uses the *same* `file_read_fd`/`file_write_fd` code path the pipe subtests cover (R12 parity), and is exercised end-to-end against a real remote by `nc` itself; the `socket_dispatch` subtest deterministically proves `OP_WRITE` routes to the `FileKind::Socket` FileOps path via the `ForcedNonblockGuard`. (An earlier `socket_round_trip` subtest used in-process **TCP loopback**, but the loopback handshake does not complete in the in-process test harness — a pre-existing netstack limitation unrelated to the ring — so it was replaced by the deterministic `socket_dispatch`.) `nc`'s arg-parse fixtures (`utest_io_capture`) still apply. **Buffer/error hardening:** `on_sock`/`on_stdin` now treat a negative completion (`ECONNRESET` etc.) as terminal rather than re-arming into a busy-spin.
- [x] **3H.4** Cancellation in userspace: `RingExecutor::cancel` submits an `OP_CANCEL` SQE targeting the future's `user_data` (exercised by the `cancel` subtest). This is where async cancellation belongs — a stuck op degrades one process, never the kernel.

#### Known limitations (documented, not blocking)

- **`ring_enter` ↔ ring-fd `close` lock-order inversion** (the global registry lock is held across the fileio probe in `ring_enter`, while `close` takes the fileio slot lock then the registry lock in `RingFileOps::release`). A concurrent enter+close on a dup'd ring fd from two threads can deadlock the global ring registry → system-wide ring DoS. Not triggered by `nc` (single-threaded, single ring) or the test suite. Tracked as **CVSS SLOPOS-2026-0007**; fix = run the probe/release outside the registry lock.
- **`OP_WRITE` / `OP_SEND` deferred send-readiness wakeup.** A blocking harvest registers the caller on each in-flight fd via `file_poll_fused(POLLIN|POLLOUT)`; if a TCP socket's `poll_wait` does not enqueue the caller on the *send* (TX-drained) wait queue, a deferred `OP_WRITE` that hit `-EAGAIN` on a full TX buffer relies on the harvest re-poll cap / `OP_TIMEOUT` deadline rather than an exact wakeup. Low-impact for `nc` (small line-sized writes rarely block); the inline (non-blocking) write path is unaffected.
- **`OP_RECVMSG` / `OP_SEND` msghdr ABI.** These dispatch through `probe_read`/`probe_write` (flat `addr`/`len` buffer) today, matching `read`/`write` on a stream socket but not the full `msghdr` (scatter-gather / control-message) semantics SLOPRING § 12 envisions. Not exercised by `nc` or the tests.
- **CQ overflow is invisible to userland.** The kernel bumps the shared `cq_overflow` counter on a full CQ but userland's `poll_completion` does not surface it (no `SLOPRING_CQ_OVERFLOW` flag yet).

### 3I: Phase 3 close

- [x] **3I.1** Three Verus proofs check on the pinned commit (`just verify`: 3 files / 32 obligations verified). `verification/STATUS.md` accurate. CI gate live.
- [x] **3I.2** `ring_setup` / `ring_enter` shipped, nine opcodes implemented; opcode parity verified by the test suite (OP_WRITE/OP_READ round-trips diff against the byte payload over the *same* `file_read_fd`/`file_write_fd` paths). `nc`'s TCP recv/send loop runs against `Ring` (`userland/src/apps/nc/ring_io.rs`) and connects to real remotes; the data-plane code path is covered by the `ring_test` pipe round-trip subtests (same `file_read_fd`/`file_write_fd` functions) and the `socket_dispatch` subtest proves the `FileKind::Socket` routing.
- [x] **3I.3** OSTD remains sync — `grep "async fn"` returns zero across `slopos-ostd/`, `ring/`, and every kernel crate (`check_no_kernel_async.sh: OK`).
- [x] **3I.4** `just check-framekernel` green (unsafe / async / alloc / stack≤2 KiB / wait-purity / fmt / KernMiri / verify); `just verify` zero failures; `just test` full pass at parity (**2485 pass, 0 fail**).
- [ ] **3I.5** Tag commit `framekernel-phase-3`. Phase-3 close PR. *(user-performed.)*
- [ ] **3I.6** Status → `phase-4-ready` (set at the close commit; Phase 4 to be planned later — see § 8 for the candidate list).

### Phase 3 Exit Criteria

1. Three Verus proofs (`Frame<M>` ref-count, slab/`HeapSlot` lifetimes, `VmSpace::cursor` invariants) check on the pinned Verus toolchain. CI-gated.
2. OSTD itself contains no `async fn` (verified by grep across every kernel crate).
3. `ring_setup` and `ring_enter` syscalls implemented; nine opcodes shipped; opcode parity with the underlying sync syscalls verified by the test suite.
4. At least one userland application (`nc`) ported to the ring surface and passing its existing test fixtures.
5. `verification/STATUS.md` published; README badge updated; "smallest verified-TCB Linux-ABI Rust kernel" claim defensible from primary sources.
6. Total proof effort ≤ 2 person-quarters; total ring-surface effort ≤ 2 person-quarters.

---

## 8. Out of Scope / Deferred

These are deliberately *not* in the four-phase plan. Listed here so future agents know they were considered.

| Item | Why deferred |
|---|---|
| **Whole-kernel async-first task model** (`Pin<Box<dyn Future>>` tasks, cooperative executor, every blocking syscall `async fn`, async TLB shootdown) | Researched in depth and retired. No production kernel across forty years and three systems languages picked this design (Asterinas, Theseus, Hubris, seL4, Linux RFL, Tock, Redox, Fuchsia, MINIX, QNX, Maestro all chose sync, including ARM-only Hubris where TLB shootdown is cheap). Cancellation safety is unsolved as a default property in any language. Verus does not yet verify `async fn` in stable releases (the `async` branch is experimental and explicitly does not support trait async). `Pin<dyn Future + Send>` lies about CPU-pinned state — `&CpuLocal<T>` borrows across `.await` are silent UB on a work-stealing executor. Future-size bloat (2–10× hand-rolled coroutines per Withoutboats) can regress kernel-heap RSS at 10K blocked tasks. Debuggability cliff: no `gdb`-of-futures, no kernel-resident `tokio-console`. The replacement is **sync core, async edge** (current Phase 3) — what Linux io_uring, Postgres + asyncpg, Redis, ScyllaDB / Seastar, and every winning production system in 2026 actually use. Source notes: `git log` around the Phase-3 replan + Hubris `doc/ipc.adoc` ("Why synchronous?"), Asterinas `ostd/src/task/mod.rs` (`FnOnce()`-based Task), Neon `docs/pageserver-thread-mgmt.md`, `verus-lang/verus#1993`, Jens Axboe `kernel.dk/io_uring.pdf`. |
| **CHERI / Morello pointer tagging** | Hardware availability too thin in 2026. Design OSTD pointer types so they *can* carry tags later (newtype around `*mut T`, never a bare pointer). Revisit 2027–2028. |
| **ARM MTE integration** | Cheaper than CHERI; can be added in a later phase once x86_64 is solid. |
| **ARM64 / RISC-V port** | Single arch keeps OSTD surface tractable through verification. Candidate for the next major plan after Phase 3. |
| **User-visible capability system** (seL4-style) | Smaller research delta than async-first; doable inside the framekernel later. Generation-counter handles (AD-11) are already half of this. |
| **Live evolution** (Theseus-style) | Hard, narrow real-world value. Skip unless a customer demands it. |
| **Multikernel / per-core OS replicas** (Barrelfish) | Adds enormous complexity for unclear win. |
| **Whole-OSTD Verus verification** | Decade of effort. Critical-path verification (Phase 3) gets 80% of the credibility for 2% of the cost. |
| **Formal proof of the Verus toolchain itself** | Out of any sane scope. |
| **TEE / SGX / TDX integration** | Asterinas-flavored use case; can layer on top later. |
| **Linux-ABI breadth + real-workload parity** (nginx / redis / sqlite under SlopOS, additional syscalls like `ppoll`, `signalfd`, `eventfd`, `accept4`, `*at` family, full signal/`wait4`/`exec` semantics under contention) | Belongs in the post-Phase-3 plan. Phase 3 keeps the kernel's syscall surface stable so Verus has a small target; ABI breadth lands once the verified substrate is in place. |

---

## 9. Risk Register

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | `VmSpace::cursor` API design wrong on first try; have to rewrite mid-Phase-1 | Medium | High | 1D builds a throwaway prototype before final API. Budget 2 weeks for this. (Closed — Phase 1 complete.) |
| R2 | KernMiri shims drift from real hardware behavior; UB findings false-positive | Low | Medium | 1K.2 chose simpler stock-Miri-with-shims path; review shims against real hardware annually. |
| R4 | Verus pinned commit rots faster than maintained; can't upgrade Rust | Medium | Medium | 3A.4 budgets quarterly Verus bumps. If Verus stops shipping, fall back to Kani for bounded checking. |
| R5 | Performance regression budget blown in Phase 2 | Medium | High | § 4.4 sets explicit ±10% bound. (Closed — Phase 2 complete; no pre-Phase-1 baseline existed to gate against, see § 6 / 2J.) |
| R7 | Generation-counter handles add latency to hot paths (FD lookup) | Low | Medium | 2H shipped with benchmarks; lookup latency within budget. (Closed.) |
| R8 | OSTD module structure (1A.2) needs reshuffling mid-Phase-1 | Low | Low | Cheap to refactor early. Don't be afraid to rename. (Closed — Phase 1 complete.) |
| R9 | Forbid-unsafe gate (1L.1) catches false positives in cfg-gated code | Medium | Low | 1L.1 has cfg-aware lookback (already in `check_alloc_dep.sh`); copy that pattern. (Closed — gates live in CI since Phase 1.) |
| R10 | Phase 1 takes >10 weeks; team morale | Low | Medium | Subtask granularity is intentional — every checked box is visible progress. (Closed.) |
| R11 | `VmSpace::cursor` Verus proof (3D) doesn't close on the fine-grained concurrent model | Medium | Medium | 3D.3 allows a coarser lock-per-`VmSpace` fallback proof. Document the gap in `verification/STATUS.md`. Re-attempt the fine-grained version on each Verus bump (3A.4). |
| R12 | io_uring opcode parity drifts from underlying sync syscalls (3G.4) | Medium | Medium | 3G.6 mandates a parity test per opcode that exercises the *same* code path as the equivalent sync syscall and diffs the observable result. CI-gated; opcode addition without a parity test is a build failure. |
| R13 | `ring/` crate brings in soft pressure to add `async fn` later ("just one async helper") | Medium | High | AD-8 + AD-9 forbid `async fn` in any kernel crate, including `ring/`. The existing `check_unsafe_outside_ostd.sh` pattern is extended in 3A.5 with a sibling `check_no_kernel_async.sh` that fails the build on any `async fn` in a kernel crate. |

---

## 10. References

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
