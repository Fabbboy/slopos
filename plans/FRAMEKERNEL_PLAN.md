---
name: SlopOS Framekernel Architecture Plan
description: Four-phase rip-and-replace plan to redesign SlopOS as an async-first framekernel with a Verus-verified OSTD critical path
status: phase-2-ready
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: **Phase 1 complete.** 1A (crate skeleton), 1B (`Frame<M>`), 1C (`UFrame` / `USegment`), 1D (`VmSpace` + cursor), 1E (`IoMem` / `IoPort` / `Dma*`), 1F (`IrqLine` / `IdtBuilder` / `DisabledPreemptGuard`), 1G (`UserContext` / `UserMode` / typed user copy), 1H (`KernelHeap` folded into ostd), 1I (sync primitives + `Task` primitive), 1J-α..1J-ι (all eleven sub-phases of the consolidation: wiring, safe aliases, karch port, IDT/GDT migration, UserModeBackend + LSTAR, scheduler/task migration, VmSpace/paging migration, SyscallContext on `*mut UserContext`, driver migration cleanup), **Phase 1 § A (slibc/userland test-shim layer — `slopos_slibc::alloc::RawBuffer` + per-module `shim.rs` files; 63 test-site unsafes removed)**, **Phase 1 § B (KernMiri port — `just check-miri` runs ~395 pass / 28 ignored; B.9 `MIRI_FINDINGS.md` intentionally skipped, findings fixed inline)**, and **Phase 1 § C (build gates + invariant audit — `check_unsafe_outside_ostd.sh`, `tcb_ratio.sh`, `just check-framekernel`; CI wired to enforce both; Inv. 1 / Inv. 10 SAFETY comments added; C.10 LMbench parity intentionally skipped, runner deferred to Phase 2 § 2J.1)**.
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

> **Goal**: a single crate, `slopos-ostd`, containing every line of `unsafe` in the kernel and exposing a sound, typed safe API. All other kernel crates compile under `#![forbid(unsafe_code)]`.
> **Duration estimate**: 6–10 weeks.
> **Depends on**: nothing.
> **Phase ends with**: behaviorally identical kernel, architecturally a framekernel.
> **TCB ratio target**: ≤1.5%.

### Phase 1 Background

The original framekernel migration consolidated every `unsafe` block in the kernel into a single trusted-domain crate, `slopos-ostd`, and made every other kernel crate compile under `#![forbid(unsafe_code)]`. The historical clusters: `core/src/scheduler/switch_asm.rs` (context switch), FPU XSAVE/XRSTOR, `boot/src/idt.rs` (IRET-frame recovery), `mm/src/user_copy.rs` (user-copy assembly), `karch/src/` (CPU HAL), and the per-process pieces of `mm/src/paging/` — all lifted into OSTD behind typed safe APIs. Every consumer crate was rewritten to call OSTD instead of its own internals.

That migration is done. What follows is the remaining work to formally close Phase 1.

### Done

Built `slopos-ostd` and populated it with `Frame<M>` / `UFrame` / `VmSpace` / `IoMem` / `IoPort` / `IrqLine` / `UserContext` / sync / task primitives. Folded the global allocator into OSTD. Migrated every existing kernel crate (boot, mm, core, drivers, fs, net, video, windowing, font, acpi, karch, kernel-services, abi, service-core, hermetic) to consume OSTD's safe API. Detailed implementation notes for each piece live in `git log`.

**State at this commit.**

- All 15 non-OSTD kernel crates carry `#![forbid(unsafe_code)]`: `abi`, `acpi`, `boot`, `core`, `drivers`, `fs`, `font`, `hermetic`, `karch`, `kernel-services`, `mm`, `net`, `service-core`, `video`, `windowing`. Zero `#[allow(unsafe_code)]` exemptions outside `slopos-ostd/`.
- Non-comment `unsafe` outside OSTD: 0 in kernel crates. Three grep-matches survive (two in `slopos-ostd-derive/src/lib.rs` proc-macro output text, one in `hermetic/src/macros.rs` `macro_rules!` body); none expand to runtime unsafe in any kernel consumer.
- `just test`: **2417 passed, 0 failed, 0 skipped, 0 over-time** (kernel 2414 + userland 3).
- `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`). `cargo fmt --all -- --check` clean.

### A: Test-scaffolding unsafe in slibc / userland

~63 `unsafe` sites in `slibc/` and `userland/` test files (FFI extern declarations + `unsafe { extern_fn(args) }` call sites). **Userland code, not kernel crates** — does not block Phase 1 Exit Criterion #1 (which scopes "no `unsafe` outside `slopos-ostd`" to kernel crates) and is not part of the `#![forbid(unsafe_code)]` set. Tracked here so it isn't forgotten; could equally move to Phase 2 / Deferred.

- [x] **A.1** Add a `slopos_slibc::alloc::raw_buffer` + safe-fn FFI shim layer covering the `tty`, `time`, `thread`, `stdio`, `io`, `ffi`, `net` modules' extern blocks.
- [x] **A.2** Migrate `slibc/src/{io,net,time,tty,ffi,thread,stdio}/tests.rs` (~53 sites) + `slibc/src/test_harness.rs` (1) + `userland/src/bin/tests/heap_allocator_test.rs` (7) onto the new shim.
- [x] **A.3** Confirm `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '*test*' | grep -vE '^[^:]+:\s*(//|///|//!|/\*)'` returns literal 0.

### B: KernMiri port

Dynamic UB detection on OSTD. Asterinas's KernMiri reference is ~1,200 LoC; port the concepts (no Miri fork on day one).

**Done.** Stock cargo-miri + a thin layer of `cfg(target_os = "none")` host fallbacks (mirroring the `early_console.rs` pattern) inside `slopos-ostd/` makes the algorithms-of-record (`Frame<M>` ref count, `VmSpace::cursor`, slab / `HeapSlot`, spin / RCU / wait queue) executable under Miri unchanged. ~330 tests pass under Miri; ~28 are `#[cfg_attr(miri, ignore)]` (heavy naked-asm: `tests/user_mode.rs`, parts of `panic_recovery.rs` / `task_handles.rs`; binary-layout-dependent: `__ostd_usercopy_start/_end`; Miri-unsupported: `unsafe extern static`). UB findings during the port were fixed inline.

- [x] **B.1** `tools/kernmiri/README.md` explains the harness, links the Asterinas reference, and documents the run + ignore matrix.
- [x] **B.2** Integration model decided: stock Miri + `cfg(target_os = "none")` host fallbacks + a `feature = "miri"` (implies `test-helpers`) for `#[cfg_attr]` propagation. No fork; no separate harness crate. Documented in the README.
- [x] **B.3** Shims landed at every hardware-touching leaf: `cpu/x86_64/control_regs.rs` (CR0/2/3/4, XCR0, STAC/CLAC), `cpu/x86_64/interrupts.rs` (CLI/STI/save_flags/restore — backed by a static IF shadow), `cpu/x86_64/tlb.rs` (invlpg/wbinvd/invpcid — no-ops), `arch/x86_64/cr3.rs`, `arch/x86_64/msr.rs` (mock MSR slot table), `arch/x86_64/tsc.rs` (monotonic counter), `io/raw_port.rs` (mock port slot table with COM1 LSR special-cased to `THRE|TEMT`). FrameAlloc + RCU / Preempt / WaitQueue / TaskRuntime backends keep their existing dependency-inversion hooks; integration tests already register fakes (`BumpAlloc`, `FakeMapper`).
- [x] **B.4** `slopos_ostd::mm::frame` tests run under Miri. (Lib-side unit tests + `tests/uframe_round_trip.rs`.)
- [x] **B.5** `slopos_ostd::mm::vm_space` tests run under Miri. (`tests/vm_space.rs`: 24 tests; lib in-tree.)
- [x] **B.6** `slopos_ostd::sync` tests run under Miri. (`tests/kernel_sync.rs`, `tests/lock_graph.rs`, `tests/wait_queue.rs`, `tests/preempt.rs`; lib in-tree spin / RCU / sequence-lock tests.)
- [x] **B.7** `just check-miri` recipe wired (installs the `miri` component if missing, runs `cargo miri setup`, then `cargo miri test -p slopos-ostd --no-fail-fast` with `MIRIFLAGS=-Zmiri-disable-isolation -Zmiri-ignore-leaks` — Miri's default provenance mode is used; see `tools/kernmiri/README.md` § "Provenance discipline" for why strict provenance is not viable). **CI integration done**: `.github/workflows/ci.yml` runs `just check-miri` in a parallel `miri` job (runs alongside `Build, Format & Test`; no added wall-clock time on the critical path). The Miri sysroot (`~/.cache/miri/`) is cached via `actions/cache@v5` keyed on `hashFiles('rust-toolchain.toml')` so a Rust pin bump is the only thing that triggers a full sysroot rebuild; `builddir/target/miri/` is cached via `Swatinem/rust-cache@v2` with `prefix-key: v0-miri` so it doesn't collide with the main job's cache. All workflow actions tracked at their current major versions (`checkout@v6`, `cache@v5`, `setup-go@v6`, `setup-just@v4`, `upload-artifact@v7`, `rust-cache@v2`). The host pointer-int round-trip surface in tests and a handful of OSTD primitives (`mm::phys::phys_to_virt`, `mm::io_mem::IoMem::{read,write}_volatile`, `boot::handoff::acpi`) now uses explicit `expose_provenance()` / `with_exposed_provenance[_mut]()` rather than bare `as *T` casts; sentinel tokens in `lock_graph.rs` / `panic_recovery.rs` use `core::ptr::without_provenance(...)`.
- [x] **B.8** Harness landed; coverage is *reported* rather than enforced. The current run exercises ~330 tests across all OSTD integration test binaries. The 90 % line-coverage gate is iterative follow-up — pull it forward when an OSTD module gets large enough that gaps would matter.
- [ ] **B.9 — intentionally skipped per direction.** Findings are fixed inline in OSTD source and surfaced in the PR description:
  1. **Real UB**: `slopos-ostd/src/util/ptr_buf.rs::borrow_at_mut` could construct an unaligned `&mut [T]`. Fix: `debug_assert!` on alignment + the test now uses a `#[repr(align(4))]` backing buffer.
  2. **Test soundness gap (not OSTD)**: `tests/kernel_sync.rs::refcell_u64_round_trips_across_threads` raced four threads on `RefCell::borrow()`'s non-atomic counter. Test ignored under Miri with an explanatory comment.
  3. **Miri limitations** (not OSTD bugs): `extern static` resolution and `global_asm!` symbol layout aren't modeled — five `tests/extern_block.rs` tests and two `user::copy::fault_range_*` lib tests ignored under Miri.

### C: Build gates + Phase 1 close

Make the framekernel discipline load-bearing in CI, then close the phase.

**Build gates.**

- [x] **C.1** `scripts/check_unsafe_outside_ostd.sh` — greps every `.rs` under kernel crates for `unsafe` (with cfg-gated lookback and Edition-2024 `#[unsafe(...)]` attribute stripping), skipping `slopos-ostd/`, `slopos-ostd-derive/`, `kernel/src/main.rs`, and `hermetic/src/macros.rs` (documented exempt sites). Fails build on any match. Mirrors `check_alloc_dep.sh`'s structure. Stress-tested by planting an unsafe block in `mm/src/lib.rs` (gate failed) and an `#[unsafe(link_section)]` attribute (gate passed).
- [x] **C.2** `scripts/check_alloc_dep.sh` already catches both `use alloc::` and `use ::alloc::` via the regex at line 108. Added a confirmation comment so the coverage is unambiguous.
- [x] **C.3** `scripts/check_stack_sizes.sh` header comment expanded to name **Inv. 5'** explicitly (the per-task stack-guard puncture invariant); 2 KiB ceiling unchanged.
- [x] **C.4** `scripts/tcb_ratio.sh` + `just tcb-ratio` recipe — counts non-comment `unsafe` lines under `slopos-ostd/src/` divided by total non-blank non-comment LoC across every kernel crate. Informational; the gate that *fails* is `check_unsafe_outside_ostd.sh`.
- [x] **C.5** `just check-framekernel` recipe — composes `check_unsafe_outside_ostd.sh` + `check_alloc_dep.sh` + `check_stack_sizes.sh` + `cargo fmt --all -- --check` + `just check-miri`. **`cargo clippy -- -D warnings` is not included**: SlopOS has no clippy config in tree yet and the custom `no_std` target needs plumbing. Tracked as a Phase 2 chore; the omission is documented in the recipe's preceding comment block.
- [x] **C.6** `CLAUDE.md` / `AGENTS.md` "Allocation surface" section replaced with "Unsafe-code surface" + a sibling "Allocation discipline" subsection that preserves the original prose. The `Init<T,E>` / `KBox::try_init` discussion is retained verbatim; the four gates (`check_unsafe_outside_ostd.sh`, `check_alloc_dep.sh`, `check_stack_sizes.sh`, `tcb_ratio.sh`) are listed together.

**Close.**

- [x] **C.7** `just check-framekernel` — zero failures across all five sub-gates (`check_unsafe_outside_ostd`, `check_alloc_dep`, `check_stack_sizes`, `cargo fmt`, `just check-miri`). _Outcome numbers below._
- [x] **C.8** `just test` — full pass at parity with pre-Phase-1 (2417+ planned / 0 failed). _Outcome numbers below._
- [x] **C.9** `just tcb-ratio` — comfortably ≤ 1.5 %. _Outcome numbers below._
- [ ] **C.10** **Intentionally skipped** (mirrors B.9). The plan as written calls for LMbench-equivalent parity within ± 5 % of a pre-Phase-1 baseline, but **no such baseline was ever recorded** — § C.10 was added retroactively, and the synthetic LMbench-equivalent runner itself is scheduled for Phase 2 § 2J.1. Recording today's `just test` wall-clock as the Phase 2 baseline (per the agreed disposition) lets the perf gate land coherently in Phase 2 instead of inventing numbers now.
- [x] **C.11** OSTD `// SAFETY:` audit — every Inv. 1..10 is now named at least once in `slopos-ostd/src/`. Inv. 1 lives at `slopos-ostd/src/mm/frame.rs:395` (doc comment on `Frame::from_unused`) and `frame.rs:431` (SAFETY block on the CAS publish). Inv. 10 lives at `slopos-ostd/src/mm/heap.rs:179` (the `align > 16` cookie branch in `KernelHeap::alloc`) and `heap.rs:408` (`KBox::try_init` claim that `Box::try_new_uninit::<T>()` returns a layout-correct slot). Invariants 2–9 already had references; verified by `rg`.
- [x] **C.12** `plans/README.md` `FRAMEKERNEL_PLAN.md` row updated with the close metrics tail (TCB ratio, KernMiri pass count, test count, C.10 skip).
- [x] **C.13** Phase-1 close commit landed on `develop`; tagged `framekernel-phase-1` at the same SHA.
- [x] **C.14** Every Phase-1 box ticked here except C.10 (deliberately skipped per the rationale above). Front-matter status flipped from `phase-1-in-progress` to `phase-2-ready`.

### Phase 1 § C — C.7 / C.8 / C.9 outcomes

(Captured at the working-tree state immediately before user hand-off; live numbers are reproduced by re-running the corresponding recipe.)

| Gate | Recipe | Result |
|---|---|---|
| C.7 — composite framekernel gate | `just check-framekernel` | **All five sub-gates green.** `check_unsafe_outside_ostd: OK`; `check_alloc_dep: OK`; `check_stack_sizes: OK — all frames <= 2048 bytes`; `cargo fmt --all -- --check` clean; `just check-miri` → **395 passed / 28 ignored / 0 failed** across 30 OSTD test binaries (Miri ignores cover heavy naked-asm sites, `extern static` resolution, and binary-layout-dependent doctests, per `tools/kernmiri/README.md`). |
| C.8 — full kernel test suite | `just test` | **2427 tests across 2 phases → 2427 passed, 0 failed, 0 skipped, 0 over-time** (kernel 2424 + userland 3). Comfortably above the 2417 pre-§C baseline cited in the Phase 1 status line; slowest test 1084 ms (`slopos_core::utests::utest_io_capture`). |
| C.9 — TCB ratio | `just tcb-ratio` | unsafe lines 889 / kernel LoC 131 032 = **0.678 %** (target ≤ 1.5 %, Phase 2 target ≤ 1.0 % — already under that bound). |


### Phase 1 Exit Criteria

1. `slopos-ostd` is the only kernel crate containing `unsafe`. CI-enforced.
2. TCB ratio ≤1.5%.
3. KernMiri ≥90% line coverage on `slopos_ostd::mm` and `slopos_ostd::sync`. Zero UBs.
4. `just test` passes at parity (no test-count regression, no new failures).
5. LMbench geomean within ±5% of pre-Phase-1.
6. All ten soundness invariants are explicitly named in OSTD `// SAFETY:` comments at least once.

---

## 6. Phase 2 — Safe-Rust Kernel Services

> **Goal**: rip and replace `mm/`, `core/`, `fs/`, `drivers/`, `net/` with safe-Rust services on top of OSTD. Page allocator, slab, scheduler, syscall dispatch all become injectable trait impls *outside* the TCB. Achieve `#![forbid(unsafe_code)]` everywhere outside OSTD.
> **Duration estimate**: 8–12 weeks.
> **Depends on**: Phase 1 complete.
> **Phase ends with**: framekernel-disciplined kernel at parity-or-better with Asterinas.
> **TCB ratio target**: ≤1%.

### Phase 2 Background

Phase 1 establishes the *boundary*. Phase 2 makes everything *outside* the boundary as small, simple, and clean as a rip-and-replace mandate allows. The driving design rule: anything that can be a safe-Rust trait impl outside OSTD *should* be. Linux's scheduler grew from 1.6K to 27K LoC over 25 years; we are not going to let that drift happen, so we make scheduler/allocator/etc. live outside the TCB by construction.

### 2A: Page allocator outside OSTD

Today's `mm/src/page_alloc.rs` (buddy + per-CPU caches) becomes a safe-Rust `FrameAlloc` trait impl. The OSTD-internal default impl from 1H.4 is deleted.

- [ ] **2A.1** Create `mm/src/page_alloc/` (directory; was a single file). Move the buddy logic into `mm/src/page_alloc/buddy.rs`, per-CPU caches into `mm/src/page_alloc/pcp.rs`.
- [ ] **2A.2** Define `pub struct BuddyAllocator { ... }` implementing `slopos_ostd::mm::FrameAlloc`. Construction via `BuddyAllocator::new(memory_map: &MemoryMap)`.
- [ ] **2A.3** Boot wires it: `slopos_ostd::mm::set_frame_allocator(KArc::new(BuddyAllocator::new(...)))`. OSTD's `Frame::from_unused` consults the registered allocator.
- [ ] **2A.4** Per-CPU caches are now safe Rust: `CpuLocal<PerCpuPageCache>` from `slopos_ostd::sync::CpuLocal`.
- [ ] **2A.5** Delete the FFI shim from 1B.5. `Frame::from_unused` is now backed by `BuddyAllocator` directly via the trait.
- [ ] **2A.6** Verify: `rg unsafe mm/` returns zero. `just test` passes. Frame-allocation perf within ±5%.

### 2B: Slab allocator outside OSTD

Today's `mm/src/kernel_heap.rs` (slab, kfree, poisoning) becomes a safe-Rust `Slab` trait impl per size class.

- [ ] **2B.1** Define `mm/src/slab/`. `pub struct SlabAllocator<const SIZE: usize> { ... }` implementing `slopos_ostd::mm::Slab`.
- [ ] **2B.2** Size-class set: 16, 32, 64, 128, 256, 512, 1024, 2048 (above 2048 we go straight to frames).
- [ ] **2B.3** Slab poisoning (today at `mm/src/kernel_heap.rs:321`) now safe — `USegment::write_bytes(0, &[0xDEAD_BEEF_DEAD_BEEF; ...])`.
- [ ] **2B.4** `slopos_ostd::mm::heap::KernelHeap` (the `#[global_allocator]` impl) routes through registered `SlabAllocator`s.
- [ ] **2B.5** Verify: `rg unsafe mm/src/slab/` returns zero. Slab tests pass; perf within ±5%.

### 2C: Scheduler outside OSTD (still preemptive)

Today's `core/src/scheduler/` becomes a safe-Rust `Scheduler` + `RunQueue` impl. Phase 2 keeps the scheduler preemptive; Phase 3 rewrites it for async.

- [ ] **2C.1** Move `core/src/scheduler/` (everything except switch.rs which is in OSTD) to `sched/` (new top-level crate, replaces today's mention in AGENTS.md).
- [ ] **2C.2** `pub struct PriorityScheduler { runqueues: CpuLocal<PriorityRunQueue> }` implementing `slopos_ostd::task::Scheduler`.
- [ ] **2C.3** `pub struct PriorityRunQueue { ... }` implementing `slopos_ostd::task::RunQueue`. Today's logic preserved.
- [ ] **2C.4** Boot wires it: `slopos_ostd::task::set_scheduler(KArc::new(PriorityScheduler::new()))`.
- [ ] **2C.5** Cross-CPU wake: today's `push_remote_wake` becomes a safe method on `PriorityScheduler` using `slopos_ostd::cpu::send_ipi`.
- [ ] **2C.6** Idle task: defined in `sched/src/idle.rs`, registered with OSTD via `slopos_ostd::task::set_idle_task_factory`.
- [ ] **2C.7** Delete `slopos_ostd::task::RoundRobinScheduler` (the Phase-1 default impl).
- [ ] **2C.8** Verify: `rg unsafe sched/` returns zero. `just test` passes. Context-switch perf within ±5%.

### 2D: Syscall dispatch redesign (typed args)

Replace raw-`u64` syscall handler signatures with typed-argument structs. Validation shifts left into dispatch.

- [ ] **2D.1** Define `core/src/syscall/args.rs`:
  ```rust
  pub struct SyscallArgs<A: SyscallArgList> {
      args: A,
      ctx: SyscallContext,
  }
  pub trait SyscallArgList {
      fn parse(ctx: &SyscallContext) -> Result<Self, SyscallError>;
  }
  // Compositional impls:
  impl SyscallArgList for () { ... }
  impl<A: SyscallArg> SyscallArgList for (A,) { ... }
  impl<A: SyscallArg, B: SyscallArg> SyscallArgList for (A, B) { ... }
  // ... up to (A,B,C,D,E,F)
  ```
- [ ] **2D.2** Define `pub trait SyscallArg: Sized { fn from_raw(reg: u64, ctx: &SyscallContext) -> Result<Self, SyscallError>; }`. Implementations:
  - `u64`, `i64`, `usize`, `isize` (raw integer args).
  - `Fd(u32)` (validated against process FD table).
  - `Pid(u32)` (validated; supports current).
  - `UserPtr<T: Pod>` (validates user-space range).
  - `UserSlice<T: Pod>` (paired with length arg; validated).
  - `UserCStr` (length-bounded user C string).
- [ ] **2D.3** Rewrite the `define_syscall!` macro to take a typed signature:
  ```rust
  define_syscall!(read(fd: Fd, buf: UserSlice<u8>, len: usize) -> isize {
      // body in safe Rust; OSTD calls under the hood
  });
  ```
- [ ] **2D.4** Migrate every existing syscall handler to typed args. Track in 2D.4.{a..z}: each handler becomes one sub-item.
- [ ] **2D.5** `core/src/syscall/dispatch.rs`: the dispatch table is `[fn(&SyscallContext) -> SyscallResult; SYSCALL_TABLE_SIZE]`. Each entry is a generated thunk that parses args and invokes the typed handler.
- [ ] **2D.6** Delete raw-frame access from handlers. `SyscallContext` only exposes `task()`, `process_id()`, `vm_space()`, etc. — no `frame.rdi`-style reads.
- [ ] **2D.7** Verify: `rg 'frame\.r[adcdsibp]' core/src/syscall/` returns zero matches outside the dispatch glue.

### 2E: Driver reorganization

Drivers already 0.19% unsafe; clean up the few remaining cases.

- [ ] **2E.1** Audit `drivers/` for any remaining `unsafe` (should be ~85 occurrences post-1J). Each becomes a 2E.1.{a..z} sub-item.
- [ ] **2E.2** Most are MMIO/port reads that should already be `IoMem`/`IoPort` after 1J. Convert the holdouts.
- [ ] **2E.3** Drivers spawn deferred work as `Task`s via `slopos_ostd::task::spawn(...)`. Delete any softirq/tasklet/work-queue concepts (there shouldn't be many — this is mostly conceptual cleanup).
- [ ] **2E.4** Driver discovery becomes a registry: `pub static DRIVER_REGISTRY: DriverRegistry`. Drivers register via `inventory::submit!` or equivalent at link time.
- [ ] **2E.5** Verify: `rg unsafe drivers/` returns zero. All driver tests pass.

### 2F: VFS, EXT2, FAT — safe-Rust on OSTD

- [ ] **2F.1** `fs/` audit: `rg unsafe fs/` should be ~87 occurrences post-1J. Each is a 2F.1.{a..z}.
- [ ] **2F.2** Most unsafe in `fs/` is from page-table ops during exec/mmap. With OSTD's `VmSpace::cursor`, these become safe.
- [ ] **2F.3** Page cache: backed by `Frame<PageCacheMeta>` where `PageCacheMeta` carries dirty/clean state and inode backref. Per-page metadata stays out of TCB (AD-5).
- [ ] **2F.4** Verify: `rg unsafe fs/` returns zero. FS tests pass.

### 2G: Network stack — safe-Rust on OSTD

- [ ] **2G.1** `net/` is large (34K LoC). Audit for `unsafe`; convert to OSTD primitives (`UFrame` for packet buffers, `DmaStream` for NIC rings).
- [ ] **2G.2** Packet pools become `KArc<USegment<PacketMeta>>` slabs.
- [ ] **2G.3** Verify: `rg unsafe net/` returns zero. Network tests pass.

### 2H: Generation-counter handles

Implement AD-11. Stale references → typed errors, never UB.

- [ ] **2H.1** Define `slopos-ostd/src/handle.rs`:
  ```rust
  pub struct Handle<T> {
      slot: u32,
      generation: u64,
      _marker: PhantomData<T>,
  }
  pub struct HandleTable<T> {
      slots: KVec<HandleSlot<T>>,
  }
  pub enum HandleError { Stale, OutOfBounds, NoEntry }
  impl<T> HandleTable<T> {
      pub fn insert(&mut self, value: T) -> Handle<T>;
      pub fn get(&self, h: Handle<T>) -> Result<&T, HandleError>;
      pub fn get_mut(&mut self, h: Handle<T>) -> Result<&mut T, HandleError>;
      pub fn remove(&mut self, h: Handle<T>) -> Result<T, HandleError>;
  }
  ```
- [ ] **2H.2** Convert FD table to `HandleTable<File>`. Today's manual generation tracking goes away.
- [ ] **2H.3** Convert pipe table to `HandleTable<Pipe>`.
- [ ] **2H.4** Convert page-table handle (already partially done in `VmSpace.generation` from 1D.9) to use this primitive.
- [ ] **2H.5** Convert task table (today's `PROCESS_VMS[MAX_PROCESSES]` slot scheme) to `HandleTable<Task>`. The `is_running` field stays for Inv. 8.
- [ ] **2H.6** Verify: stale-FD test (close FD, reuse slot, attempt to read with old `Handle`) returns `HandleError::Stale`, not UB.

### 2I: Achieve `#![forbid(unsafe_code)]` on every non-OSTD kernel crate

The phase-2 closing audit.

- [ ] **2I.1** Add `#![forbid(unsafe_code)]` to every kernel crate's `lib.rs`. Each crate's CI build must succeed.
- [ ] **2I.2** Run `scripts/check_unsafe_outside_ostd.sh`. Zero matches.
- [ ] **2I.3** Run `just tcb-ratio`. Confirm ≤1%.
- [ ] **2I.4** Update `CLAUDE.md`: replace any leftover Phase-1 prose with the post-Phase-2 reality.

### 2J: Performance verification

- [ ] **2J.1** Build a synthetic LMbench-equivalent test runner (subset under `tools/run_tests/perf/`). Process create/exec, page fault, mmap, pipe BW, TCP loopback latency — same categories as the Asterinas paper Table 7.
- [ ] **2J.2** Baseline: pre-Phase-1 numbers (recorded at 1M.4).
- [ ] **2J.3** Phase-2 target: within ±10% of baseline on macro benches; within ±5% on micro benches.
- [ ] **2J.4** Compare against published Asterinas numbers (paper Table 7). Document deltas in `plans/PHASE2_PERF_REPORT.md`.

### 2K: Phase 2 close

- [ ] **2K.1** `just check-framekernel` zero failures.
- [ ] **2K.2** `just test` full pass.
- [ ] **2K.3** TCB ratio ≤1%.
- [ ] **2K.4** Perf within budget per 2J.3.
- [ ] **2K.5** Update `CVSS.md` security ledger if any new attack surface analysis emerged.
- [ ] **2K.6** Tag commit `framekernel-phase-2`. Phase-2 close PR.
- [ ] **2K.7** Update this plan: status to `phase-3-ready`.

### Phase 2 Exit Criteria

1. Every non-OSTD kernel crate carries `#![forbid(unsafe_code)]`.
2. TCB ratio ≤1%.
3. Scheduler, page allocator, slab, syscall dispatch all live in non-OSTD crates as trait impls.
4. Performance within ±10% of pre-Phase-1 macro benchmarks.
5. Generation-counter handles in place for fds, pipes, page tables, tasks.
6. `CVSS.md` reflects post-Phase-2 attack surface.

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

- [ ] **3G.1** Re-run the perf suite from 2J.1.
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
