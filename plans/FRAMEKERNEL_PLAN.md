---
name: SlopOS Framekernel Architecture Plan
description: Three-phase rip-and-replace plan to redesign SlopOS as a framekernel with a Verus-verified OSTD critical path and an io_uring-style async edge
status: phase-3-ready
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: **Phase 1 & 2 complete.** `slopos-ostd` owns every line of kernel `unsafe`; all 17 non-OSTD kernel crates are `#![forbid(unsafe_code)]`; TCB ratio 0.722 % (target ≤1 %). Tagged `framekernel-phase-1`; `framekernel-phase-2` close commit pending. **Phase 3 (Verus-verified OSTD + io_uring-style async edge) is next.** Per-subphase implementation notes live in `git log`.
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

- [ ] **3D.1** Read the CortenMM paper + open-source proofs (`http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf`). Notes file `verification/notes/cortenmm.md`.
- [ ] **3D.2** Adapt CortenMM-style invariants to `slopos_ostd::mm::vm_space::Cursor`:
  - "Cursor operations preserve page-table well-formedness (no dangling intermediate frames; every entry points at a valid PTE for the cursor's lifetime)."
  - "Concurrent cursors over non-overlapping virtual ranges do not interfere (range-disjoint transactionality)."
  - "Mapping a `UFrame` increments its `ref_count` exactly once; unmapping decrements exactly once. Inv. 4 + Inv. 5 hold across the operation."
- [ ] **3D.3** Prove. Concurrent cursors likely require RCU-style or epoch-style proof obligations. Falling back to a coarser lock-per-`VmSpace` proof is acceptable if the fine-grained one doesn't close — document the gap.
- [ ] **3D.4** Replace source. `just build` + `just test` clean.
- [ ] **3D.5** `verification/STATUS.md` updated.

### 3E: Public proof status

- [ ] **3E.1** `verification/STATUS.md`: which OSTD modules are **verified**, which are **audited only**, which are **unaudited**. Per-module: proof file link, spec summary, pinned Verus SHA.
- [ ] **3E.2** Status badge in `README.md`: *"OSTD critical path: 3/3 proofs check on Verus &lt;SHA&gt;"*.
- [ ] **3E.3** Public claim, defensible from primary sources: **SlopOS is the smallest verified-TCB Linux-ABI Rust kernel.** No other kernel today meets all four adjectives (small TCB, verified, Linux-ABI, Rust).

### 3F: io_uring-style ring surface — design

Now Verus is set; the kernel's verified-sync substrate is the platform the userspace async story sits on.

- [ ] **3F.1** Design doc as `plans/SLOPRING.md` *before* any code. Cover:
  - SQ/CQ ring layout (head/tail indices, slot format, ABI-stable opcodes).
  - Memory: rings live in a per-process `Frame<RingMeta>` shared between kernel and userspace via `VmSpace::cursor` — mapped read/write to user, read/write to kernel; ownership stays with the process.
  - Submission model: userspace writes SQEs, calls `ring_enter(ring_fd, to_submit, ...)` (one new syscall). Kernel snapshots head, processes SQEs, posts CQEs, advances tail. **All sync from the kernel's perspective.**
  - Completion model: kernel posts CQEs synchronously when the underlying sync op completes; for ops that block (read on empty pipe, accept on empty queue), the kernel arms an `EventBus` wait on behalf of the SQE and the CQE is posted from the existing `WaitQueue` wake path. Userspace polls or `ring_wait`s on the CQ.
  - Cancellation: opcode `OP_CANCEL`. The kernel walks its in-flight table, removes the matching `WaitQueue` registration, posts a CQE with `-ECANCELED`. No async-fn cancellation hazard — the kernel side is straight-line sync. Async cancellation belongs in userspace, where stuck futures degrade one process and not the whole kernel.
  - Backpressure: SQ-full → user retries; CQ-full → kernel drops with `IORING_CQ_OVERFLOW` flag (matches Linux semantics).
- [ ] **3F.2** Justify each opcode in terms of an existing sync syscall path. `OP_READ` reuses `fs_read`; `OP_WRITE` reuses `fs_write`; `OP_RECVMSG` reuses `unix_recvmsg` / `socket_recvmsg`; `OP_SEND` reuses `socket_send` / `unix_send`; `OP_ACCEPT` reuses `socket_accept` / `unix_accept`; `OP_POLL_ADD` reuses `file_poll_register_fd`; `OP_TIMEOUT` reuses the sleep-queue path; `OP_NOP` for benchmarks. **No new blocking primitives.**
- [ ] **3F.3** Document the threat model: SQEs are user-controlled bytes; every field is validated; the ring memory is a `UFrame<RingMeta>` (Inv. 4 + Inv. 5 hold by construction); no kernel reference into the ring outlives a single `ring_enter` invocation.

### 3G: io_uring-style ring surface — implementation

- [ ] **3G.1** New crate `ring/` (kernel-side) carrying `#![forbid(unsafe_code)]`. Hosts the SQ/CQ snapshot, opcode dispatch, in-flight table. Allocations via `slopos-ostd` discipline.
- [ ] **3G.2** Two new syscalls in `abi/src/syscall/numbers.rs`: `ring_setup(entries: u32, flags: u32) -> RingFd` and `ring_enter(ring_fd, to_submit: u32, min_complete: u32, flags: u32) -> i32`. Implemented via `define_syscall!` — both **sync**. Threaded through the existing dispatch path; no executor turn.
- [ ] **3G.3** Per-ring kernel state stored in the `HandleTable` shape from Phase 2H (generation-counter handles, so stale ring FDs return a typed error, never UB).
- [ ] **3G.4** Opcodes implemented. Each is one subtask (3G.4.{a..i}): `OP_READ`, `OP_WRITE`, `OP_RECVMSG`, `OP_SEND`, `OP_ACCEPT`, `OP_POLL_ADD`, `OP_TIMEOUT`, `OP_CANCEL`, `OP_NOP`.
- [ ] **3G.5** `min_complete > 0`: kernel waits on the CQ's internal `WaitQueue` (existing `BUS.subscribe(...).wait_event(...)` machinery) until `>= min_complete` CQEs are posted or signal pending → `EINTR`. The waiter is the user task; no kernel-side future.
- [ ] **3G.6** Test crate `ring/tests/`: KTAP-grammar tests for opcode parity (each opcode produces the same observable result as the equivalent sync syscall under identical input), backpressure, cancellation, ring-FD inheritance across `fork`, kill-during-`ring_enter` signal handling.

### 3H: Userspace async edge

The ring surface is useless without a userspace runtime that consumes it. This sub-phase builds the smallest credible one.

- [ ] **3H.1** New userland crate `slibc-ring/` (under `userland/` or a new top-level): `pub struct Ring`, `pub fn setup(entries) -> Ring`, `Ring::submit(sqe) -> SubmissionToken`, `Ring::wait_completion()`, `Ring::poll_completion()`. Mirrors `liburing`'s shape.
- [ ] **3H.2** A `slopfut` async runtime (or vendored embassy-style executor — pick at 3H.1 time) that drives `Ring` completions onto futures. **This is userland** (`#![forbid(unsafe_code)]` does not apply — userland is outside the kernel discipline per `CLAUDE.md`).
- [ ] **3H.3** Port one real app to use it: `userland/src/apps/nc/` (already exists and already non-trivial) — its `recv` / `send` loop becomes `Ring`-driven. Demonstrates the edge end-to-end.
- [ ] **3H.4** Cancellation semantics in userspace: dropping a future submits an `OP_CANCEL` SQE for the in-flight op. This is where async cancellation belongs.

### 3I: Phase 3 close

- [ ] **3I.1** Three Verus proofs check on the pinned commit. `verification/STATUS.md` accurate. CI gate live.
- [ ] **3I.2** `ring_setup` / `ring_enter` shipped, opcode parity verified by the test suite, `nc` runs against `Ring`.
- [ ] **3I.3** OSTD remains sync (`rg "async fn" slopos-ostd/` returns zero matches; same check across every kernel crate).
- [ ] **3I.4** `just check-framekernel` zero failures; `just verify` zero failures; `just test` full pass at parity.
- [ ] **3I.5** Tag commit `framekernel-phase-3`. Phase-3 close PR.
- [ ] **3I.6** Status → `phase-4-ready` (Phase 4 to be planned later; out of this document's scope — see § 8 for the candidate list).

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
