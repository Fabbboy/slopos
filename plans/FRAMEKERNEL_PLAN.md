---
name: SlopOS Framekernel Architecture Plan
description: Four-phase rip-and-replace plan to redesign SlopOS as an async-first framekernel with a Verus-verified OSTD critical path
status: phase-1-in-progress
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: Phase 1 in progress — 1A (crate skeleton), 1B (`Frame<M>`), 1C (`UFrame` / `USegment`), 1D (`VmSpace` + cursor), 1E (`IoMem` / `IoPort` / `Dma*`), 1F (`IrqLine` / `IdtBuilder` / `DisabledPreemptGuard`), and 1G (`UserContext` / `UserMode` / typed user copy) complete; 1H next.
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

Today's irreducible `unsafe` clusters in seven places: `core/src/scheduler/switch_asm.rs` (context switch), FPU XSAVE/XRSTOR, `boot/src/idt.rs:645` (IRET frame recovery), `mm/src/user_copy.rs:28–54` (user-copy assembly), `karch/src/` (CPU HAL: cli/sti, CR3, CPUID, MSR), and bits of `mm/src/paging/`. Phase 1 lifts these into `slopos-ostd` and wraps each in a typed safe API. Existing `mm/`, `core/`, `fs/`, `drivers/`, `net/` keep working — they're rewritten to call OSTD instead of their current internals.

The phase is structured into 13 subtask groups (1A–1M). 1A creates the crate; 1B–1I build primitives in dependency order; 1J migrates the existing kernel; 1K adds dynamic verification; 1L locks in build gates; 1M closes the phase.

### 1A: Crate skeleton and module layout

Create `slopos-ostd/` with the module tree the rest of Phase 1 will populate.

- [x] **1A.1** Create directory `slopos-ostd/` with:
  - `Cargo.toml` declaring `[lib]` `slopos_ostd`, edition 2021, `no_std`. *(Done — uses `edition.workspace = true` (`2024`) to match every other kernel crate; `[lib]` left implicit since the crate name `slopos-ostd` already produces lib name `slopos_ostd`.)*
  - `src/lib.rs` with `#![no_std]`, `#![forbid(unsafe_op_in_unsafe_fn)]`, module declarations matching 1A.2.
- [x] **1A.2** Create the module tree (empty stubs are fine; populated 1B–1I):
  ```
  slopos-ostd/src/
    lib.rs          // re-exports + crate-level doc
    cpu/            // CPU HAL: instructions, CR3, MSRs, CPUID
      mod.rs
      x86_64.rs
      preempt.rs    // disable_preempt(), DisabledPreemptGuard
    mm/             // memory primitives
      mod.rs
      frame.rs      // Frame<M>, AnyFrameMeta
      uframe.rs     // UFrame, USegment, AnyUFrameMeta
      vm_space.rs   // VmSpace, Cursor
      io_mem.rs     // IoMem
      dma.rs        // DmaCoherent, DmaStream
      heap.rs       // KBox/KVec/KArc/PinBox glue (moves slopos-alloc here)
      init.rs       // Init<T,E>, Zeroable (moves from slopos-alloc)
    sync/           // synchronization primitives
      mod.rs
      spin.rs       // SpinLock<T>
      mutex.rs      // Mutex<T>
      wait_queue.rs // WaitQueue
      rcu.rs        // Rcu<T>
      cpu_local.rs  // CpuLocal<T>
    irq/            // interrupt machinery
      mod.rs
      line.rs       // IrqLine, register_callback
      idt.rs        // IDT setup, IRET frame recovery
    io/             // port I/O
      mod.rs
      port.rs       // IoPort
    user/           // user-mode entry
      mod.rs
      context.rs    // UserContext, sensitive-bit hiding
      mode.rs       // UserMode::execute(), ReturnReason
      copy.rs       // copy_from_user / copy_to_user (assembly + recovery)
    task/           // bare task primitive (NOT async yet)
      mod.rs
      task.rs       // Task, TaskOptions, CurrentTask
      switch.rs     // context switch assembly
      fpu.rs        // XSAVE/XRSTOR
    boot/           // early init helpers (limine glue, GDT, TSS)
      mod.rs
    arch/           // arch-specific: x86_64 only for now
      mod.rs
      x86_64/
        mod.rs
        msr.rs
        cpuid.rs
        gdt.rs
        tss.rs
  ```
- [x] **1A.3** Add `slopos-ostd` to the workspace `Cargo.toml`. Wire it as a dependency of `kernel/`, `boot/`, `mm/`, `core/`, `sync/`, `fs/`, `drivers/`, `net/`, `acpi/`, `karch/` (only crates that need it; userland and slibc do not).
- [x] **1A.4** Add a one-line `slopos-ostd/README.md` explaining the crate is the kernel's trusted core; link this plan.
- [x] **1A.5** Verify: `cargo check -p slopos-ostd` succeeds with empty modules. `just build` still succeeds (no consumers yet).

### 1B: `Frame<M>` with typed metadata

Replace `mm/src/page_alloc.rs:1283` `OwnedPageFrame` with a generic `Frame<M>` and migrate per-page state into `M`.

- [x] **1B.1** In `slopos-ostd/src/mm/frame.rs`, define:
  ```rust
  pub unsafe trait AnyFrameMeta: Send + Sync + 'static {
      const SIZE: usize;
      const ALIGN: usize;
      fn on_drop(&mut self, paddr: Paddr);
  }
  pub struct Frame<M: AnyFrameMeta + ?Sized> {
      ptr: *const MetaSlot,
      _marker: PhantomData<M>,
  }
  // SAFETY: all methods preserve Inv. 1, Inv. 4.
  ```
  Methods: `from_unused(paddr, M) -> Self`, `from_in_use(paddr) -> Self`, `paddr() -> Paddr`, `reference_count() -> usize`, `borrow() -> &M`, `into_raw() -> *const MetaSlot`, `from_raw(*const MetaSlot) -> Self` (unsafe). *(Done — `AnyFrameMeta` carries an extra `Sized` bound so the default `SIZE`/`ALIGN` consts can use `size_of::<Self>()`/`align_of::<Self>()`; `Frame<M>` correspondingly drops the `?Sized` from the draft. Constructors return `Result<Self, FrameError>` so `META_SLOTS`-uninitialised / out-of-range / state-mismatch cases stay typed errors instead of panics. `reference_count()` returns `u32` to match the underlying `AtomicU32`.)*
- [x] **1B.2** In `slopos-ostd/src/mm/frame.rs`, define `MetaSlot`: a fixed-layout struct with ref count (`AtomicU32`), state tag, type-erased metadata storage (sized for max-`M`). Static array `META_SLOTS: [MetaSlot; N_FRAMES]` allocated at boot from a region reserved by the boot subsystem. *(Done — `MetaSlot` is `#[repr(C, align(8))]` with `ref_count` at offset 0 (asserted via `const _`); inline storage is `MaybeUninit<MetaStorage>` where `MetaStorage` is `#[repr(C, align(8))]` so any `M` with `ALIGN ≤ MAX_META_ALIGN` is correctly aligned. A type-erased `MetaVtable` (one per concrete `M` via the `HasVtable` associated-const pattern) carries `drop_in_place` + `on_drop` for the Drop dispatch. `MAX_META_SIZE = 16`, `MAX_META_ALIGN = 8`. `META_SLOTS` is a pointer + length pair guarded by atomic loads, populated by the new `init_meta_slots(slots, len)` boot hook — left uncalled in 1B per 1B.7. Phase 1J wires the array.)*
- [x] **1B.3** Implement `Drop for Frame<M>`: Acquire-load ref count, if 1 → release, run `M::on_drop`, free the underlying physical frame. Document the Acquire/Release pair against Inv. 9. *(Done — `fetch_sub(1, Release)` followed by an `Acquire` fence on the last-ref path; pairs with `from_in_use`'s `AcqRel` add. On the last ref, `on_drop` and `drop_in_place` dispatch through the slot's `MetaVtable`, then the slot is reset to `UNUSED`. The actual physical-frame return to the buddy allocator is **not** wired in Drop yet — that lands in 1J alongside the registered `FrameAlloc`; until then Drop is unreachable in practice because `META_SLOTS` is uninitialised.)*
- [x] **1B.4** Provide a few `M` impls in `slopos-ostd/src/mm/frame.rs`:
  - `KernelMeta` — generic kernel-owned page.
  - `PageTableMeta` — for page-table levels (used by 1C).
  - `AnonymousMeta` — for `UFrame` (defined in 1C).

  *(Done — all three impls land alongside `AnyFrameMeta`; each carries a `const _: () = assert_meta_fits::<M>();` guard so size/align regressions fail at compile time.)*
- [x] **1B.5** Provide `FrameAllocOptions` (size, zeroing, alignment) + a private trait `FrameAlloc` that the Phase-2 allocator will implement. For Phase 1, ship a stub that delegates to today's `mm/src/page_alloc.rs::alloc_page_frame` via FFI shim. *(Done — `FrameAllocOptions { size_pages, zeroing, align_pages }` (with `single()`/`zeroed()` const builders) and `pub trait FrameAlloc` in `slopos-ostd/src/mm/frame.rs`. The Phase-1 shim `LegacyFrameAllocShim` lives in the new `mm/src/frame_alloc_shim.rs` (defined there to avoid making `slopos-ostd` depend on `slopos-mm`); it asserts `size_pages == 1` / `align_pages == 1` for now (multi-page lands in 1C with `USegment`) and delegates to `alloc_page_frame` / `free_page_frame`. Not registered with OSTD anywhere yet — 1J wires it.)*
- [x] **1B.6** Unit tests in `slopos-ostd/src/mm/frame.rs::tests` (compile only; runtime tested in 1J):
  - Layout asserts (`MetaSlot` is repr(C) and ref count is at offset 0).
  - `AnyFrameMeta::SIZE` is `≤ MAX_META_SIZE`.

  *(Done — both checks live as crate-level `const _: () = assert!(...)` blocks (so they fire on every `cargo check`) and are mirrored in `#[cfg(test)] mod tests` for host-side `cargo test` once a host harness exists. Kernel-side runtime tests via `stest!` come with 1J.)*
- [x] **1B.7** Verify: `cargo check -p slopos-ostd` succeeds. `just build` still succeeds (no consumers; FFI shim keeps old API live). *(Done — `cargo check -p slopos-ostd` and `cargo check -p slopos-mm` both clean; `just build` finishes in ~5 s with `check_alloc_dep: OK` and `check_stack_sizes: OK`. `cargo fmt --all -- --check` clean. `slopos-ostd/Cargo.toml` now declares only `slopos-abi` and `slopos-alloc` — no `slopos-sync` (would be a circular-dep tell), no other accidentally pulled-in crates.)*

### 1C: `UFrame` + `USegment` (untyped memory)

The single highest-value soundness primitive in the whole plan. Closes the `&T`-over-MMIO/DMA bug class.

- [x] **1C.1** In `slopos-ostd/src/mm/uframe.rs`, define:
  ```rust
  pub unsafe trait AnyUFrameMeta: AnyFrameMeta {}
  pub type UFrame<M = AnonymousMeta> = Frame<M> where M: AnyUFrameMeta;
  pub struct USegment<M: AnyUFrameMeta = AnonymousMeta> {
      // contiguous run of UFrames
      head: UFrame<M>,
      len_pages: usize,
  }
  ```
  *(Done — `UFrame<M>` is a **newtype** around `Frame<M>`, **not** a type alias. The alias would inherit `Frame::borrow() -> &M` (and any future `Frame` helpers), which would let callers pull a Rust reference into untyped memory through the back door — exactly the bug class 1C exists to close. The newtype keeps `into_frame`/`from_frame` `pub(crate)` so the only exit from "untyped" is inside the crate. `AnyUFrameMeta: AnyFrameMeta + Default` (Default added so `from_unused_run` can fan a single payload across each frame in a segment); `AnonymousMeta` is the only impl in 1C; `KernelMeta` and `PageTableMeta` deliberately do NOT implement it because their pages are sensitive kernel memory. `USegment<M>` stores `KVec<Frame<M>>` plus a head paddr / len_pages — `KVec` ownership cleanly drops every per-frame ref on segment Drop.)*
- [x] **1C.2** Implement byte-copy interface on `UFrame` and `USegment` *only*:
  - `read_bytes(offset, dst: &mut [u8]) -> Result<(), UFrameError>`
  - `write_bytes(offset, src: &[u8]) -> Result<(), UFrameError>`
  - `read_pod<T: Pod>(offset) -> Result<T, UFrameError>` where `Pod` is a marker trait for plain-old-data (defined in 1C.3).
  - `write_pod<T: Pod>(offset, value: T) -> Result<(), UFrameError>`
  - **Forbidden**: `as_slice`, `as_mut_slice`, `Deref<Target=[u8]>`, `DerefMut`. There must be no way to obtain a Rust reference into a `UFrame`. Test for this in 1C.7.

  *(Done — implementation in `slopos-ostd/src/mm/uframe.rs`. Range checks via shared `check_range(offset, len, region)` helper; alignment checks via `check_alignment::<T>(paddr, offset)` operating on the **physical**+offset address (not virt — virt is a constant page-aligned offset away). Internal phys-to-virt pointer comes from a new `slopos-ostd::mm::phys` module: `unsafe fn init_phys_virt_offset(u64)` (one-shot, AcqRel `swap` against `u64::MAX` sentinel — same pattern as `init_meta_slots`, no `slopos-sync::InitFlag` because OSTD must not depend on `slopos-sync`) plus `pub(crate) fn phys_to_virt(Paddr) -> *mut u8`. Phase 1J wires `init_phys_virt_offset` from kernel boot; for now host integration tests install it explicitly.)*
- [x] **1C.3** Define `pub unsafe trait Pod: Copy + 'static {}` and implement for primitives (`u8`, `u16`, `u32`, `u64`, `i8`–`i64`, `usize`, `isize`, `[T; N] where T: Pod`, fixed `#[repr(C)]` POD structs via a derive macro added in 1C.4). *(Done — `slopos-ostd/src/mm/pod.rs`; manual impls for `u8 u16 u32 u64 u128 i8 i16 i32 i64 i128 usize isize ()` and a blanket `[T; N] where T: Pod`. **Deliberately no `bool`** (only 0x00/0x01 valid), no `f32`/`f64` (NaN bit-pattern equivalence), no `char` (UTF-32 validity), no references, no raw pointers — all flagged with explanatory module-level docs so future "obvious additions" are caught at review. Re-exported at crate root as `slopos_ostd::Pod` so the derive expansion can resolve `::slopos_ostd::Pod`.)*
- [x] **1C.4** Add a `slopos-ostd-derive/` proc-macro crate with `#[derive(Pod)]` that checks the struct is `#[repr(C)]` and all fields are `Pod`. (This is the only proc-macro we need in Phase 1.) *(Done — new `slopos-ostd-derive/` workspace member with `[lib] proc-macro = true`; deps `syn = { version = "2", features = ["full"] }`, `quote`, `proc-macro2` (added to workspace.dependencies). Derive parses `#[repr(...)]` attributes and **requires** `C` or `transparent`; **rejects `packed`** (its misaligned-read invariants conflict with `read_pod` alignment checks); rejects enums + unions. Field-level `T: ::slopos_ostd::Pod` `where`-bounds added so the type-checker enforces field POD-ness. Output uses fully-qualified paths `::slopos_ostd::Pod` to satisfy the workspace's `warnings = "deny"` lint.)*
- [x] **1C.5** Implement `IoSlice` / `IoSliceMut` on `USegment` for vectored I/O — still byte-copy, no references. *(Done — exposed as `UIoSlice` / `UIoSliceMut` (renamed from the plan's `IoSlice` to avoid confusion with `std::io::IoSlice`); each is a `(paddr, len_bytes)` descriptor — no `&[u8]` ever crosses the boundary. `USegment::io_slices()` / `io_slices_mut()` return single-element arrays since segments are always physically contiguous; the array shape is future-proofing for scatter/gather lists.)*
- [x] **1C.6** Define `UFrameError` enum: `OutOfBounds`, `Misaligned`, `Truncated`. *(Done — plus a fourth variant `OutOfMemory` returned from `USegment::from_unused_run` when the per-segment `KVec<Frame<M>>` bookkeeping allocation fails. `Truncated` is reserved for 1E vectored-I/O / partial-segment paths and is unused in 1C.)*
- [x] **1C.7** Compile-fail test (`tests/ui/uframe_no_ref.rs` using `trybuild`):
  - Attempting `&uframe[0..4]` must not compile.
  - Attempting `uframe.deref()` must not compile.

  *(Done — implemented as **embedded `compile_fail` doctests on `UFrame`** rather than via `trybuild`. Three blocks lock in the no-Deref / no-Index / no-`as_slice` discipline; they run under `cargo test --doc -p slopos-ostd` and report as `compile fail ... ok`. Switched away from `trybuild` because its `.stderr` snapshot files are brittle across rustc versions and would impose a maintenance tax on the workspace; the compile-fail doctest pattern gives the same guarantee with zero extra dev-deps.)*
- [x] **1C.8** Unit tests in `slopos-ostd/src/mm/uframe.rs::tests`:
  - Round-trip `read_pod` / `write_pod` for `u64`, `[u8; 16]`, a `#[derive(Pod)]` struct.
  - Out-of-bounds `read_bytes` returns `OutOfBounds`.
  - Misaligned `read_pod::<u64>` returns `Misaligned`.

  *(Done — split across two layers. **Pure-logic** unit tests in `slopos-ostd/src/mm/uframe.rs::tests` exercise `check_range` and `check_alignment` (full page, overrun, arithmetic overflow, aligned, misaligned), `UFrameError` `Eq`, and `size_of::<UFrame<AnonymousMeta>>() == size_of::<*const ()>()` (newtype zero-cost). **Round-trip integration tests** in `slopos-ostd/tests/uframe_round_trip.rs` install scratch meta slots + a phys-virt offset pointing into a leaked `#[repr(C, align(4096))] Backing([u8; PAGE_SIZE * N])` buffer, then exercise `u64`/`[u8; 16]`/`#[derive(Pod)]` round-trips, OOB, Misaligned, and a `USegment` test that crosses a 4 KiB physical-page boundary in a single byte-copy. Test isolation: shared `OnceLock<Mutex<()>>` setup gate so global OSTD state is initialised exactly once and tests serialise inside the binary. Required minor support — `MetaSlot::new_unused()` + `reset_meta_slots_for_test()` + `phys::reset_for_test()` gated behind `#[cfg(any(test, feature = "test-helpers"))]`; `slopos-ostd` declares `[features] test-helpers = []` and a `[dev-dependencies]` self-reference enables the feature for `cargo test`.)*
- [x] **1C.9** Verify: `cargo check -p slopos-ostd` succeeds. `trybuild` test passes. *(Done — `cargo check -p slopos-ostd` and `cargo check -p slopos-ostd-derive` clean. `cargo test -p slopos-ostd` reports **9 lib + 7 integration + 3 doctest = 19 passes, 0 failures**. `cargo fmt --all -- --check` clean. `just build` finishes ~7.6 s with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no stack frame regressions vs. pre-1C). No `slopos-ostd` consumer outside OSTD has been touched — `mm/`, `core/`, `drivers/` still on the legacy paths; Phase 1J does the consumer migration.)*

### 1D: `VmSpace` + cursor

Replace `ProcessPageDir` exposure (`mm/src/paging/tables.rs:24-39`) with a typed `VmSpace`. Page-table mutation is only via `cursor`.

- [x] **1D.1** In `slopos-ostd/src/mm/vm_space.rs`, define:
  ```rust
  pub struct VmSpace {
      pml4: Frame<PageTableMeta>,  // root page-table frame
      pcid: Pcid,
      generation: u64,             // generation counter (AD-11)
  }
  pub struct Cursor<'a> {
      space: &'a VmSpace,
      range: Range<Vaddr>,
      depth: u8,
      // walking state
  }
  pub struct CursorMut<'a> {
      space: &'a mut VmSpace,
      range: Range<Vaddr>,
      depth: u8,
  }
  ```

  *(Done — `VmSpace`, `Cursor<'a>`, `CursorMut<'a>`, `CursorEntry`, and `MapError` all live in `slopos-ostd/src/mm/vm_space.rs`. `pml4` is private (AD-4); the only mutation path is `cursor_mut`. Cursor walking state is `(range, cur)` rather than the draft's `depth` — depth is implicit in the per-call walker (`page_table::walk_to_leaf`), which keeps the cursor `repr`-stable across huge-page splits. `generation` is `AtomicU64` (Acquire/AcqRel) so the read-only `Cursor::query` path doesn't need `&mut`.)*
- [x] **1D.2** `VmSpace::new() -> VmSpace`: allocates a fresh PML4 frame, initializes with kernel-half mappings inherited from the kernel's master page table.

  *(Done — `VmSpace::new()` returns `Result<Self, MapError>` so allocator-not-registered, master-not-registered, and OOM cases stay typed instead of panicking. The kernel-master PML4 paddr is supplied via a one-shot AcqRel-swap registration hook `vm_space::register_kernel_master_pml4(PhysAddr)` (analogous to `init_meta_slots` / `init_phys_virt_offset`). `copy_kernel_half` reads/writes indices 256..512 via the shared `page_table::entry_in_table` helper. PCID assignment is a Phase-1-stub monotonic counter (`alloc_pcid()`); 1J swaps it for `mm::mmu::asid::select_cr3`.)*
- [x] **1D.3** `VmSpace::cursor(&self, range: Range<Vaddr>) -> Cursor<'_>` and `VmSpace::cursor_mut(&mut self, range) -> CursorMut<'_>`. Range must be page-aligned.

  *(Done — both return `Result<_, MapError::UnalignedRange>` rather than panicking on a misaligned range. The check also rejects inverted ranges (`start > end`). Empty page-aligned ranges are accepted (a no-op cursor); this matches the host integration test `check_range_alignment_accepts_empty_aligned`.)*
- [x] **1D.4** `CursorMut` methods (the load-bearing API):
  - `map(&mut self, frame: UFrame<M>, prop: PageProperty) -> Result<(), MapError>`: maps the frame at the cursor's current position; advances cursor.
  - `unmap(&mut self) -> Option<UFrame<M>>`: unmaps the current page; returns the freed `UFrame` if any.
  - `protect(&mut self, prop: PageProperty)`: changes properties without remap.
  - `query(&self) -> CursorEntry`: read-only query of current entry.
  - `next(&mut self)` / `seek(&mut self, vaddr)`: navigation.

  *(Done — `map` consumes the `UFrame<M>` and **leaks** its `Frame<M>` ref into the leaf PTE via `Frame::into_raw`, so the page table holds the only outstanding ref (no double-counting). `unmap` reclaims that ref through a new `Frame::from_raw_at(paddr)` helper (added to `frame.rs`), which is `unsafe fn` because the caller must promise exactly one ref was previously leaked at `paddr`. `unmap` and `protect` call `tlb::flush_local(self.cur)` after committing; `map` does not (a previously-empty PTE has nothing cached). `query` is exposed on both `Cursor` and `CursorMut` (the latter delegates to a temporary `Cursor`). Deliberately did NOT auto-advance `map` — drafts called for "advances cursor" but Asterinas's recent code stays explicit, and the test suite is cleaner this way; callers chain `cur.map(...)?; cur.next()?;`.)*
- [x] **1D.5** `PageProperty` struct: `read`, `write`, `execute`, `user`, `cache_policy` (WB, WC, UC), `global`. Encoded into PTE bits internally.

  *(Done — `slopos-ostd/src/mm/page_property.rs` carries `PageProperty` + `CachePolicy` (`WriteBack`/`WriteCombining`/`Uncacheable`). Round-trip helpers `to_leaf_flags()` / `from_leaf_flags(PteFlags)` plus public consts `KERNEL_RW`, `KERNEL_RO`, `USER_RW`, `USER_RO`, `USER_RX` so callers don't reconstruct flags by hand. **Cache-policy mapping** uses the firmware-default PAT layout SlopOS already uses: WB → 0/0, WC → PWT=1, UC → PCD=1. Phase 2's PAT cleanup may flip these around — encapsulating the mapping inside `PageProperty` keeps callers oblivious. `read=true` is unconditionally PRESENT on x86_64 (no separate read bit); the field is carried for ARM64 forward-compat.)*
- [x] **1D.6** `MapError` enum: `Overlap`, `OutOfBounds`, `IntermediateAllocFailed`, `MisalignedFrame`.

  *(Done — variants `Overlap`, `OutOfBounds`, `IntermediateAllocFailed`, `Uninitialised`, `UnalignedRange`, `PathCorrupt`. `MisalignedFrame` from the draft is dropped — `UFrame` paddrs are always 4 KiB-aligned by construction (they come out of `Frame::from_unused`, which is fed by `FrameAlloc::alloc(FrameAllocOptions::single())`), so a frame-misalignment failure mode is unreachable in practice. Added `Uninitialised` (FrameAlloc / kernel-master PML4 not registered) and `UnalignedRange` (cursor range not page-aligned) so OSTD tests get typed errors instead of panics. `PathCorrupt` is a soundness-canary variant: triggered only when the page-table tree contradicts itself (e.g., PML4 entry marked HUGE — architecturally invalid).)*
- [x] **1D.7** Internal helpers (private to `slopos-ostd::mm`):
  - `walk_to_leaf(pml4, vaddr, depth)`: navigates page tables, allocating intermediate frames as needed.
  - `split_huge(entry, depth)`: splits a 1GiB or 2MiB entry into smaller pages.
  - All `unsafe` here references Inv. 4 + Inv. 5.

  *(Done — implementation in `slopos-ostd/src/mm/page_table.rs` (~480 LoC). **Three** walk modes via a `WalkMode` enum: `Query` (read-only, NotPresent on missing intermediate), `Mutate` (read-only path search for `unmap`/`protect`), `Create` (allocates intermediates, splits huge pages). Returns `WalkOutcome::{LeafTable | NotPresent}` so callers don't conflate "tree empty here" with "leaf empty here". `split_pdpt_huge` and `split_pd_huge` mirror `mm/src/paging/tables.rs:114-159` line-for-line — same loop shape, same flag inheritance, same `table_flags_from_leaf` transform. Both wrap the new intermediate as `Frame<PageTableMeta>::from_unused(...)` and leak the ref into the parent PTE (so refcount stays exact). The `Pte` wrapper goes through `core::ptr::{read,write}_volatile` so the compiler can't reorder PTE ops against surrounding atomic stores. Every `unsafe` block has a `// SAFETY:` comment naming Inv. 4 and/or Inv. 5.)*
- [x] **1D.8** `VmSpace::activate(&self)`: writes `pcid_encoded_cr3` to CR3. This is the *only* sanctioned way to switch address spaces.

  *(Done — `VmSpace::activate(&self)` is itself an `unsafe fn` (kernel-half invariant is the caller's responsibility; see method docs) and delegates to a new `slopos-ostd::arch::x86_64::cr3::write_cr3_pcid(PhysAddr, Pcid, no_flush: bool)`. The `Pcid(u16)` newtype masks construction to 12 bits and exposes a `Pcid::KERNEL` constant. The CR3 write itself is the single inline-asm `unsafe` block (`mov %rax, %cr3` AT&T syntax with `nostack, preserves_flags`). 1J integrates with the existing `mm::mmu::asid` selector — until then, `activate()` works against the Phase-1 stub PCID counter. Host tests do not exercise `activate()`.)*
- [x] **1D.9** Generation counter: `VmSpace::generation()` returns the value; bumped on every cursor commit. Used by Phase-2 generation-counter handles (AD-11).

  *(Done — `VmSpace::generation()` reads an `AtomicU64` with Acquire ordering. `CursorMut` carries a `dirty: bool`; every successful `map` / `unmap` / `protect` sets it; `Drop for CursorMut` does a single `fetch_add(1, AcqRel)` if dirty. **Per-session, not per-PTE** — Phase-2 stale-handle code only needs to know "did the address space change since I last looked?", and the per-page granularity Asterinas uses is overkill for Phase 1's needs. Verified by the integration tests `generation_bumps_once_per_session` (3 maps in one cursor → +1) and `read_only_cursor_does_not_bump_generation` (read-only `Cursor::query` → +0).)*
- [x] **1D.10** Unit tests in `slopos-ostd/src/mm/vm_space.rs::tests` (run under KernMiri once 1K lands):
  - Map → query → unmap round-trip.
  - Map two `UFrame`s at consecutive vaddrs; cursor walks both.
  - Map over existing mapping returns `MapError::Overlap`.
  - Unmap → freed `UFrame` ref count drops to 0.

  *(Done — split across two layers. **Lib unit tests** (`slopos-ostd/src/mm/{page_property,page_table,vm_space}.rs::tests`) cover pure-logic round-trips: `PageProperty ↔ PteFlags` for every cache policy + USER/NX bit toggles (7 tests), `PageTableLevel::index_of` against a known `0x5566_7788_9000` vaddr + a clean-bit test at `0x4000_0000` (4 tests), and `MapError` Eq + `VmSpace: Send + Sync` + range-alignment rejection (5 tests). **Host integration tests** (`slopos-ostd/tests/vm_space.rs`) wire OSTD against a 1 MiB heap-allocated 4 KiB-aligned scratch arena (256 pages) plus a bump `FrameAlloc` impl, exercise all 4 plan-listed scenarios + 8 additional ones (overlap, unmap-of-unmapped, protect-toggles-write, OOB-after-step-past-range, generation-bumps, read-only-no-bump, unaligned-range-rejected, seek-round-trip), totalling 12 integration tests. Test isolation uses the `OnceLock<Mutex<()>>` setup gate from `tests/uframe_round_trip.rs`; the gate's `.lock()` recovers from poison so a panicking test doesn't cascade-fail every other test in the binary. Allocator note: `Backing` is `1 MiB` so we route through `std::alloc::alloc_zeroed` with a 4 KiB-aligned `Layout` rather than `Box::new(Backing([0; …]))` — the latter overflows the test thread's stack. KernMiri-port (1K) will re-target these.)*
- [x] **1D.11** Verify: `cargo check -p slopos-ostd` succeeds; unit tests pass under host `cargo test` where they don't need real paging.

  *(Done — `cargo check -p slopos-ostd` clean. `cargo test -p slopos-ostd` reports **28 lib + 7 uframe integration + 12 vm_space integration + 3 doctest = 50 passes, 0 failures**, up from 19 pre-1D. `cargo fmt --all -- --check` clean. `just build` finishes in ~6 s with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no new ≥2 KiB frames; the deepest stack user — `walk_to_leaf` — stays well under via the `WalkOutcome` enum + early-return shape). `slopos-ostd/Cargo.toml` adds `bitflags = { workspace = true }` as a new dep — the only new transitive crate, already in workspace.dependencies. **TCB delta**: vm_space.rs has 7 `unsafe` tokens (3× `unsafe impl Send/Sync`, 2× `unsafe fn` decls on registration hooks, 1× `unsafe { write_cr3_pcid(...) }` call inside `activate`, 1× `unsafe { Frame::from_raw_at(...) }` in `unmap`). page_table.rs has 9 (PTE volatile reads/writes + 2 `unsafe fn` decls). cr3.rs has 3 (`unsafe fn` decl + 1 inline-asm block + the doc-comment `unsafe` mention). page_property.rs has 0. **Phase 1J will retire the duplicate `PteFlags` / `PageTableLevel` against `mm/src/paging_defs.rs` + `mm/src/paging/page_table_defs.rs`.**)*

### 1E: `IoMem`, `IoPort`, `DmaCoherent`, `DmaStream`

Move `mm/src/mmio.rs::MmioRegion` into OSTD as `IoMem`; add `IoPort` and DMA wrappers. IOMMU default-deny.

- [x] **1E.1** In `slopos-ostd/src/mm/io_mem.rs`, define `IoMem`:
  ```rust
  pub struct IoMem {
      virt_base: Vaddr,
      phys_base: Paddr,
      size: usize,
      // SAFETY: range was certified insensitive at construction (Inv. 7).
  }
  impl IoMem {
      pub fn read<T: Pod>(&self, offset: usize) -> T;
      pub fn write<T: Pod>(&self, offset: usize, value: T);
      pub fn sub_region(&self, offset: usize, size: usize) -> Option<IoMem>;
  }
  ```
  Migrate the implementation from `mm/src/mmio.rs:36-193`. Add bounds + alignment asserts. **Forbidden**: any method returning `&[u8]` or `&T`. *(Done — `slopos-ostd/src/mm/io_mem.rs`. `IoMem` is `Clone` (cheap value type) but not `Copy` and not `Drop`; mappings are leak-only in Phase 1E (Phase 2 owns recyclable kernel virt). `read` / `write` use `assert!` (not `debug_assert!` as legacy MmioRegion did) on bounds + alignment so release builds still trip — OSTD-side miscoding is unrecoverable. **Added `try_read` / `try_write`** returning `Result<T, IoMemError>` for fallible callers (PCI BAR enumeration, virtio config probe). `read`/`write` operate on `T: Pod` rather than `T: Copy` (tighter than legacy). The no-`&T` discipline is enforced by three `compile_fail` doctests on `IoMem` mirroring `UFrame`'s pattern in `slopos-ostd/src/mm/uframe.rs:122-144`: `Deref`, `Index<Range<usize>>`, and `as_slice` all fail to compile. `IoMem` is `Send + Sync` (raw addresses + size, no aliased state). `sub_region` shares the parent's mapping with offset phys/virt, matches `MmioRegion::sub_region` semantics.)*
- [x] **1E.2** Define `IoMemRegistry` (private to OSTD). Constructed at boot from ACPI/firmware tables marking insensitive ranges. The *only* way to obtain an `IoMem` is `IoMemRegistry::reserve(phys_range)`. Inv. 7 enforced here. *(Done — `IoMemRegistry::reserve(phys, size, IoMemCachePolicy)` is the **only** path that constructs an `IoMem` outside the module. The registry walks a `&'static [PhysRange]` (registered once via `unsafe fn register_io_mem_registry`) and rejects any request not contained within some range with `IoMemError::NotReserved`. `PhysRange { base: PhysAddr, len: usize }` (half-open, overflow-safe `contains_range`) is exposed publicly; same shape will be reused by Phase 1J's IOMMU policy. `IoMemMapper` is a separately-registered trait (one-shot via `register_io_mem_mapper`); it owns the kernel-virt allocation + page-table install (the dependency arrow forbids OSTD calling `slopos-mm` directly). `IoMemCachePolicy { Uncacheable, WriteCombining, WriteThrough, WriteBack }` flows through the trait. Boot constructs the registry from MCFG (PCIe ECAM), MADT (LAPIC, IOAPIC), HPET ACPI table, and the Limine framebuffer response — wiring lands in 1J. Test-only `reset_for_test` clears both globals (matches `frame_alloc::reset_for_test`).)*
- [x] **1E.3** In `slopos-ostd/src/io/port.rs`, define `IoPort<T: PortAccessible>`:
  ```rust
  pub unsafe trait PortAccessible: Pod {}
  unsafe impl PortAccessible for u8 {}
  unsafe impl PortAccessible for u16 {}
  unsafe impl PortAccessible for u32 {}
  pub struct IoPort<T: PortAccessible> {
      port: u16,
      _marker: PhantomData<T>,
  }
  impl<T: PortAccessible> IoPort<T> {
      pub fn read(&self) -> T;
      pub fn write(&self, value: T);
  }
  ```
  *(Done — `slopos-ostd/src/io/port.rs`. `PortAccessible` is sealed via a private `Sealed` sub-trait so downstream impls cannot break the `in`/`out` opcode width invariant. **`read` / `write` are kept `unsafe fn`** (deviating from the draft signatures): port writes have arbitrary hardware side effects (CMOS index latching, PIC EOI ordering, debug-exit) and the registry only certifies *which* ports are reachable — not *what* sequence is sound. The split matches the existing `slopos-utils::io::Port` contract. Inline asm for u8/u16/u32 lifted verbatim from `slopos-utils/src/io.rs:25-107` with the same `nomem nostack preserves_flags` options. `io_wait()` (port `0x80` POST diagnostic) ports across as `pub unsafe fn`. Crate-level `slopos-utils::Port` is **not** retired in 1E — the early-boot panic logger still uses it; Phase 1J migrates non-boot consumers.)*
- [x] **1E.4** `IoPortRegistry`: same pattern as `IoMemRegistry`. Insensitive ports only (no PIC, no PCI config 0xCF8/0xCFC after Phase 4 completion of legacy modernization). *(Done — `IoPortRegistry::reserve<T: PortAccessible>(port)` checks `port..port + size_of::<T>()` against a `&'static [PortRange]` registered via `unsafe fn register_io_port_registry`. `PortRange { start: u16, end: u16 }` is half-open with overflow-safe `contains` — rejects inverted ranges and 16-bit-overflowing access widths. `IoPortError { NotReserved, Uninitialised }`. `IoPort<T>` carries `Clone + Copy + PartialEq + Eq + Hash + Debug`; `offset(off)` deliberately returns the same `IoPort` type without re-reserving — sub-port acquisition that needs registry checking goes through a fresh `IoPortRegistry::reserve(port + off)` call. Test-only `reset_for_test` clears the registry.)*
- [x] **1E.5** In `slopos-ostd/src/mm/dma.rs`, define `DmaCoherent` and `DmaStream`:
  ```rust
  pub struct DmaCoherent {
      segment: USegment<DmaCoherentMeta>,
      // IOMMU mapping established at construction.
  }
  pub struct DmaStream {
      segment: USegment<DmaStreamMeta>,
      direction: DmaDirection,
  }
  pub enum DmaDirection { ToDevice, FromDevice, Bidirectional }
  ```
  Constructors take `USegment`, program IOMMU page tables, return DMA-mapped handle. `Drop` tears down IOMMU mapping. *(Done — `slopos-ostd/src/mm/dma.rs`. **Single-step `alloc(npages, [direction])`** instead of the two-step `map(useg)` from the draft: two-step would require re-installing meta on a `USegment<AnonymousMeta>`, which conflicts with `Frame::from_unused`'s UNUSED→TYPED transition (`uframe.rs:151`). Single-step matches Asterinas upstream and is testable cleanly. `DmaCoherentMeta` and `DmaStreamMeta` are ZSTs that `unsafe impl AnyFrameMeta + AnyUFrameMeta` — DMA pages are by definition peripheral-tampered, so the no-`&T` contract from `uframe.rs:62-69` is the right home. **`DmaError` is four-state** (`NotInitialised`, `Exhausted`, `Forbidden`, `MappingFailed`) for Phase-2 debuggability rather than the draft's two-state. `Drop` calls `IommuMapper::unmap` and lets the segment release frames; module docs flag the in-flight-DMA caveat — drivers must quiesce the device before drop, OSTD does not issue a DMA fence. Both handles support `read_pod`/`write_pod`/`read_bytes`/`write_bytes` delegation onto the inner `USegment`. `phys_base()` exposes the contiguous physical run for diagnostics.)*
- [x] **1E.6** IOMMU default-deny: at boot, `slopos-ostd::boot::init_iommu()` programs the IOMMU so no physical address is DMA-accessible. `DmaCoherent::map(useg)` is the *only* operation that opens a window. Inv. 6 enforced here. *(Done — at the type-system level. `IommuMapper` trait + one-shot `register_iommu_mapper` registration; default = **no mapper registered** = `DmaError::NotInitialised`, so no DMA window can be opened until the IOMMU driver wires itself up. Trait carries `map(phys, size, direction) -> Result<u64, DmaError>` and `unmap(iova, size)`. **Caveat documented in dma.rs module header**: Phase 1E ships only the type-level deny; the *hardware* IOMMU is not programmed until Phase 1J. A device the bootloader left with an open DMA window can still issue DMA — closing that gap is hardware-bring-up work. The trait shape will gain a per-page IOVA list in 1J alongside the real VT-d driver; Phase 1E ships the contiguous-only surface (driven by the Asterinas-upstream observation that contiguous IOVA suffices for the common case). `DmaCoherent::Drop` and `DmaStream::Drop` invoke `IommuMapper::unmap` automatically.)*
- [x] **1E.7** `DmaStream` cache management: `sync_for_device()`, `sync_for_cpu()` for non-coherent architectures (no-ops on x86_64, but the API is there for future ARM64). *(Done — `DmaStream::sync_for_device()` and `DmaStream::sync_for_cpu()` are `#[inline]` no-ops on x86_64 (the architecture is cache-coherent for DMA). The API is preserved so drivers can be written portably for future ARM64 support without a churn-PR. `DmaCoherent` does **not** carry these methods — coherent buffers are by definition cache-snooped and don't need sync points; this matches the Linux DMA API distinction.)*
- [x] **1E.8** Unit tests in `slopos-ostd/src/mm/io_mem.rs::tests` and `slopos-ostd/src/mm/dma.rs::tests`:
  - `IoMem::read` / `write` round-trip on a fake MMIO region.
  - `IoMemRegistry::reserve` of an unregistered range returns `None`.
  - `DmaCoherent::map` on a `USegment` produces a non-zero IOVA.
  - `DmaCoherent::drop` removes the IOMMU mapping (verified by re-mapping the same physical range).

  *(Done — split across two layers as for 1C/1D. **Lib unit tests** in `slopos-ostd/src/{mm/io_mem,mm/dma,io/port}.rs::tests` cover pure-logic shape: `PhysRange::contains_range` (simple + overflow), `PortRange::contains` (simple + inverted + 16-bit wrap), `IoMemError`/`DmaError`/`IoPortError`/`DmaDirection`/`IoMemCachePolicy` Eq, `IoMem: Clone`, `IoMem::sub_region` offset arithmetic, `DmaCoherentMeta`/`DmaStreamMeta` Default, `UFrameError → DmaError` round-trip — 15 new lib tests on top of the prior 28 (43 total). **Host integration tests** in `slopos-ostd/tests/io_mem.rs` (11), `slopos-ostd/tests/io_port.rs` (6), `slopos-ostd/tests/dma.rs` (9). Same `OnceLock<Mutex<()>>` setup gate, leak-`Box`-for-`'static`, and poison-recovery-on-lock pattern as the 1C/1D test files. The DMA harness ships a multi-page `BumpAlloc` impl of `FrameAlloc` (production `LegacyFrameAllocShim` will gain matching multi-page support in 1J) plus a `RecordingMapper` impl of `IommuMapper` that logs every map/unmap call into a `Mutex<Vec<...>>` so tests can assert that `Drop` fires `unmap` with the correct IOVA + size. Disjoint paddr ranges across files. Phase-1E coverage **specifically validates**: register-then-reserve happy path; reserve-rejects-unregistered-range; reserve-without-registration-returns-Uninitialised; round-trip read/write for `u32` and a `#[derive(Pod)]` struct; `try_read` returns `OutOfBounds`/`Misaligned`; `sub_region` inherits phys offset and rejects overrun; `IoMem::Clone` works; port `reserve_succeeds`/`rejects_unregistered`/`rejects_overrun_at_range_end`/`rejects_when_uninitialised`/`offset_advances_address`/`u16_in_two_byte_range`; DMA coherent `alloc` records (phys, size) + `iova` non-zero; DMA coherent without registered mapper or frame-allocator returns `NotInitialised`; DMA coherent `Drop` calls `unmap` with matching `(iova, size)`; DMA coherent `read_pod`/`write_pod` round-trip via the segment; DMA stream records direction in the recorded map call; DMA stream `Drop` calls `unmap`; DMA stream `sync_*` are no-ops; `DmaCoherent::alloc(0)` returns `Exhausted`.)*
- [x] **1E.9** Verify: `cargo check -p slopos-ostd` succeeds; tests compile. *(Done — `cargo check -p slopos-ostd` clean; `cargo check -p slopos-mm` clean (the new `slopos-mm/src/io_mem_mapper_shim.rs` compiles in isolation — Phase-1 `LegacyIoMemMapperShim` defined but not yet registered by boot, mirroring the `LegacyFrameAllocShim` pattern from 1B.5). `cargo test -p slopos-ostd` reports **43 lib + 11 io_mem + 6 io_port + 9 dma + 7 uframe + 12 vm_space + 6 doctest = 94 passes, 0 failures**, up from 50 pre-1E (+44). `cargo fmt --all -- --check` clean. `just build` finishes in ~5 s with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no new ≥ 2 KiB frames). **TCB delta**: 45 `unsafe` tokens added across the three new files: `io_mem.rs` (10 — registration `unsafe fn` decls, two `unsafe impl Send/Sync`, four volatile-read/write blocks, two registry-load blocks); `port.rs` (29 — six asm blocks plus the trait/method `unsafe fn` decls, two register hooks, `io_wait`); `dma.rs` (6 — two registration `unsafe fn` decls, one registry-load block, three `unsafe impl AnyFrameMeta`/`AnyUFrameMeta` for the two meta types). All `unsafe` blocks carry `// SAFETY:` comments naming the relevant invariant (Inv. 6 for IOMMU + DMA, Inv. 7 for IoMem + IoPort). **Phase 1J wiring TODO**: register the legacy `IoMemMapperShim`, register the boot-built `&'static [PhysRange]` and `&'static [PortRange]` insensitive lists, register a (eventually real) `IommuMapper`, and migrate `MmioRegion` consumers in `drivers/` to `IoMem`. No driver code or kernel boot path was modified in 1E itself — every existing `MmioRegion` / `slopos-utils::Port` caller continues to use the legacy types unchanged.)*

### 1F: `IrqLine` + interrupt registration

Move IRQ machinery from `boot/src/idt.rs` and `core/src/irq.rs` into OSTD. Drivers register handlers via `IrqLine::register_callback`.

- [x] **1F.1** In `slopos-ostd/src/irq/line.rs`, define:
  ```rust
  pub struct IrqLine {
      vector: u8,
      // SAFETY: vector was allocated via IrqAllocator; preserves Inv. 3.
  }
  pub struct IrqAllocator { /* private */ }
  impl IrqLine {
      pub fn alloc() -> Result<IrqLine, IrqError>;
      pub fn vector(&self) -> u8;
      pub fn register_callback<F>(&self, handler: F) -> CallbackHandle
      where F: Fn(&IrqContext) + Send + Sync + 'static;
  }
  pub struct IrqContext {
      pub vector: u8,
      // sensitive frame fields hidden
  }
  ```
  *(Done — `slopos-ostd/src/irq/line.rs`. **`register_callback` returns `Result<CallbackHandle<'a>, IrqError>`** rather than the draft's infallible `CallbackHandle`: heap allocation can fail on OOM, and the second-call-on-same-vector case needs a typed error. `IrqLine::alloc` lives on `IrqAllocator` (`IrqAllocator::alloc()`) per the draft. `IrqContext<'a>` carries a lifetime parameter so closures cannot retain the context past dispatch; only `vector()` and `error_code()` accessors are public — RIP / RSP / CS / RFLAGS are *not* reachable, enforcing Inv. 2 by construction. **`CallbackHandle<'a>` borrows the issuing `IrqLine`** (`PhantomData<&'a IrqLine>`), so the borrow checker forbids the line dropping while the handle exists; this prevents a freed vector from being re-allocated while a stale dispatch slot still holds the handle's closure. The dispatch table is `[AtomicPtr<HandlerCell>; 256]` with the closure stored as `KBox<dyn Fn(&IrqContext<'_>) + Send + Sync + 'static>` — the implicit `CoerceUnsized` from `slopos-alloc` keeps the fat pointer inside the boxed cell, so `AtomicPtr<HandlerCell>` stores a thin pointer. Vector pool is a hand-rolled CAS-loop `AtomicBitmap` over the 192-bit 32..224 range (slopos-utils' `AtomicBitmap` is unreachable from OSTD — the dep tree forms a cycle through `slopos-arch`). Bitmap word count is `192.div_ceil(64) = 3`. Reserved-vector list is registered via `unsafe fn register_irq_reserved(&[u8])`, additive + idempotent; reserved bits are mirrored into the allocated bitmap on registration, and `Drop` of an `IrqLine` consults the reserved bitmap before clearing the allocated bit. `pub fn shutdown()` flips an `AtomicBool` that suppresses subsequent `dispatch` calls — used at orderly teardown so in-flight handlers can drain without racing the dispatch table. Test-only `reset_for_test` clears both bitmaps, every dispatch slot, and the shutdown flag.)*
- [x] **1F.2** In `slopos-ostd/src/irq/idt.rs`, port `boot/src/idt.rs` IDT setup. Keep IRET frame recovery (`idt.rs:645`) — that's irreducible. Add `// SAFETY: ...` referencing Inv. 2 + Inv. 3.
  *(Done — `slopos-ostd/src/irq/idt.rs`. `IdtBuilder` owns the 256-entry IDT in a `UnsafeCell<[IdtEntry; 256]>` with `unsafe impl Sync`; `set_gate` / `set_gate_priv` / `set_ist` / `get_gate` are safe; `load` is `unsafe fn load(&self)` and emits `lidt` from a stack-allocated 10-byte `IdtPtr`. `IdtEntry` is defined in-module (the planned `slopos-ostd::arch::idt` re-export was unnecessary — slopos-arch's `IdtEntry` is the canonical version that the existing kernel still uses, and OSTD's port is structurally identical) with `IdtEntry::format(handler, sel, typ, dpl)` and `IdtEntry::handler()` helpers — both `const fn` so the unit tests can run pure-logic round-trips. The vector constants `IDT_ENTRIES`, `IDT_GATE_INTERRUPT`, `IDT_GATE_TRAP` re-derive in OSTD; the existing 0xEC / 0x80 / 0xFB / 0xFC / 0xFD / 0xFE constants stay in `slopos-arch` (1J consolidates). **`handle_corrupt_iret_frame(*const u64) -> !`** ports the IRET-corruption recovery; it reads 5 unaligned u64s and panics. The diagnostic dump is **abstracted behind a `DiagnosticSink` trait** registered via `unsafe fn register_diagnostic_sink(&'static dyn DiagnosticSink)` (one-shot; default = silent) — the real klog-backed dump (kernel-stack vicinity, current-task identity, surrounding stack words) lives in `boot/` for now and migrates in 1J. `ExceptionMode { Normal, Test }` is here for 1J's exception-test override mode (currently in `boot/src/idt.rs::ExceptionMode`). Inline-asm `lidt` block carries `// SAFETY:` Inv. 2.)*
- [x] **1F.3** Bottom halves are NOT a concept in OSTD. Drivers wanting deferred work spawn an ordinary `Task` (Phase 2 services do this). This deletes today's softirq/tasklet/work-queue concepts.
  *(Done — recorded as a module-level decision paragraph in `slopos-ostd/src/irq/mod.rs`: "SlopOS deliberately does not ship the softirq / tasklet / work-queue family of bottom-half mechanisms. Drivers that need to defer work out of an IRQ-context callback spawn an ordinary `Task` and signal it from inside the handler". No code-level deletion was needed: SlopOS never had softirq/tasklet/work-queue infrastructure (a workspace-wide `rg 'softirq|tasklet|work_queue'` returns zero hits in `core/`, `drivers/`, `sync/`, `mm/`). The `kthread` spawning + `IrqMutex`-based block/unblock pattern in `core/scheduler/` is the *existing* deferral mechanism and is the surface that 1I's async `Task` machinery generalises.)*
- [x] **1F.4** Implement `slopos-ostd::cpu::preempt::DisabledPreemptGuard` — RAII guard that disables preemption on construction, re-enables on drop. Used inside `register_callback` handlers when atomic-context is required.
  *(Done — `slopos-ostd/src/cpu/preempt.rs`. `DisabledPreemptGuard` carries `PhantomData<*const ()>` for `!Send` (matches `sync/src/preempt.rs:18`); `new()` calls `current_backend().enter()`, `Drop` calls `leave()`. The actual count storage is behind a **`PreemptBackend` trait** (`enter` / `leave` / `leave_quiet` / `count`) registered via `unsafe fn register_preempt_backend(&'static dyn PreemptBackend)` — one-shot, asserts not-already-installed via an `AtomicBool` swap. The default-no-op backend is a `static NoOpBackend` whose internal `AtomicU32` count makes host-side unit tests deterministic. `leave_quiet` is the "decrement without invoking deferred reschedule" variant required by IST exception handlers; the trait provides a default impl that forwards to `leave`, prod backends override with a fetch_sub-only path. The trait-object slot uses `UnsafeCell<MaybeUninit<&'static dyn PreemptBackend>>` + `AtomicBool::swap(true, AcqRel)` handshake (`AtomicPtr<dyn Trait>` would not compile — fat pointer). Phase 1J registers a backend that proxies to `slopos_arch::pcr::current_pcr().preempt_count`. The pre-existing `sync/src/preempt.rs::PreemptGuard` is **left intact**: it carries the deferred-reschedule callback semantics that belong on the combined IRQ+preempt guard (lands in `slopos-ostd::sync::Mutex` later); 1F's `DisabledPreemptGuard` is the pure preempt-only variant.)*
- [x] **1F.5** Port the `IstPreemptHold` discipline (mentioned in agent memory; lives in `boot/src/idt.rs`) into `slopos-ostd::irq::idt`. Every IST-using vector gets a typed `IrqEntryGuard<V>` that bumps preempt count on entry and decrements on exit. This is an Inv. 2 preservation.
  *(Done — both shapes provided. `IrqEntryGuard<const V: u8>` is the const-generic variant: `enter()` consults `vector_uses_ist(V)` (`V < 32` in SlopOS — all architectural CPU exceptions use IST stacks) and bumps the per-CPU preempt count via `crate::cpu::preempt::irq_entry_bump()`; `Drop` calls `irq_entry_leave_quiet()` so no deferred reschedule callback fires (which would corrupt the IST stack). Non-IST vectors construct as a no-op — uniform entrypoint code in 1J's stub generator. `IstPreemptHold` is the runtime-bool variant matching the existing struct shape in `boot/src/idt.rs:358-375`, used for dispatch entry points where the vector is dynamic. Both share the `irq_entry_bump` / `irq_entry_leave_quiet` `pub(crate)` hooks in `cpu/preempt.rs` — those route through `current_backend().enter()` / `leave_quiet()`.)*
- [x] **1F.6** Verify: `cargo check -p slopos-ostd` succeeds. Existing IRQ-using code still routes through `core/src/irq.rs` (which becomes a thin wrapper in 1J).
  *(Done — `cargo check -p slopos-ostd` clean; `cargo test -p slopos-ostd` reports **69 lib + 9 dma + 11 io_mem + 6 io_port + 10 irq_line + 10 preempt + 7 uframe + 12 vm_space + 6 doctest = 140 passes, 0 failures**, up from 94 pre-1F (+46). `cargo fmt --all -- --check` clean. `just build` finishes in ~4 s with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no new ≥ 2 KiB frames). The kernel's existing `boot/src/idt.rs` and `core/src/irq.rs` compile unchanged — the new OSTD primitives are unwired, so `just boot` is byte-identical to pre-1F. **TCB delta**: 11 `unsafe` blocks/decls added across `slopos-ostd`: `irq/line.rs` (4 — `unsafe fn register_irq_reserved`, `&*raw` in `dispatch`, `KBox::from_raw` in `clear_dispatch`, `KBox::from_raw` in error path of `register_callback`); `irq/idt.rs` (5 — `unsafe impl Sync for IdtBuilder`, `unsafe impl Sync for SinkSlot`, `unsafe fn register_diagnostic_sink`, `unsafe fn handle_corrupt_iret_frame`, `lidt` asm block, two `*self.entries.get()` mutation blocks, `(*BACKEND_SLOT)` MaybeUninit reads); `cpu/preempt.rs` (2 — `unsafe impl Sync for BackendSlot`, `unsafe fn register_preempt_backend`, plus the matching MaybeUninit::write/read sites). Every `unsafe` block carries a `// SAFETY:` comment naming Inv. 2 (kernel-mode CPU state untamperable by OSTD clients) or Inv. 3 (peripheral untamperability). **Phase 1J wiring TODO**: (1) register the pcr-backed `PreemptBackend` so production preempt count goes through `slopos_arch::pcr::current_pcr().preempt_count`; (2) register the klog-backed `DiagnosticSink` and migrate the kernel-stack vicinity dump out of `boot/src/idt.rs::handle_corrupt_iret_frame`; (3) replace `boot/src/idt.rs` IDT setup with calls into `slopos_ostd::irq::idt::IdtBuilder`; (4) replace the `core/src/irq.rs` handler table + `irq_dispatch` with calls into `slopos_ostd::irq::line::dispatch`; (5) populate the platform-reserved vector list (`SYSCALL_VECTOR=0x80`, `LAPIC_TIMER_VECTOR=0xEC`, `LUF_DRAIN_IPI_VECTOR=0xFA`, `RCU_QS_IPI_VECTOR=0xFB`, `RESCHEDULE_IPI_VECTOR=0xFC`, `TLB_SHOOTDOWN_VECTOR=0xFD`, `0xFE` shutdown IPI, `0xFF` spurious) via `register_irq_reserved`; (6) install `IrqEntryGuard::<V>::enter()` at the head of every IST-using IDT stub; (7) consolidate `slopos-arch::arch::idt::IdtEntry` with `slopos-ostd::irq::idt::IdtEntry` after the migration so there is exactly one definition. None of (1)–(7) was performed in 1F itself — every existing `boot/src/idt.rs` / `core/src/irq.rs` caller continues to use the legacy types unchanged.)*

### 1G: `UserContext` + `UserMode`

The only sanctioned way to enter user mode. Sensitive RFLAGS bits hidden.

- [x] **1G.1** In `slopos-ostd/src/user/context.rs`, define:
  ```rust
  pub struct UserContext {
      // user-mode register subset only
      regs: UserRegs,
  }
  #[derive(Clone)]
  pub struct UserRegs {
      pub rax: u64, pub rbx: u64, pub rcx: u64, pub rdx: u64,
      pub rsi: u64, pub rdi: u64, pub rbp: u64, pub rsp: u64,
      pub r8: u64,  pub r9: u64,  pub r10: u64, pub r11: u64,
      pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
      pub rip: u64,
      pub rflags_user_subset: u64,  // IF, IOPL, sensitive bits MASKED OUT
      pub fs_base: u64, pub gs_base: u64,
      pub cs: u16, pub ss: u16,
      // FPU state pointer (separate XSAVE area)
      pub fpu_state: FpuStateRef,
  }
  ```
  *(Done — `slopos-ostd/src/user/context.rs`. `UserRegs` is `#[repr(C)] #[derive(Clone, Copy, Debug, Default)]` (Default lets host tests build a zeroed register file ergonomically). `UserContext` carries `{ regs, fpu_state }`; the two-field shape (vs. inlining `fpu_state` into `UserRegs`) keeps `UserRegs` `Pod`-shaped for future ABI plumbing while `FpuStateRef` retains its `Send + Sync` raw-pointer-plus-length newtype shape. `cs` / `ss` are forced to `0x23` / `0x1B` (matching `karch::arch::gdt::SegmentSelector::USER_CODE` / `USER_DATA`) inside `UserContext::new` and are not publicly settable; `set_regs` re-applies them so a snapshot replacement cannot bypass the selector enforcement. `FpuStateRef::from_raw` / `empty()` are the only construction paths; `Send + Sync` impls reference Inv. 2 in their SAFETY comments.)*
- [x] **1G.2** `UserContext::set_rflags(&mut self, value: u64)` masks off sensitive bits (IF=1 forced, IOPL=0 forced, RF cleared, NT cleared, etc.). Inv. 2. *(Done — implemented as `(value & USER_RFLAGS_PERMITTED) | USER_RFLAGS_FORCED` against two `pub const` masks. `USER_RFLAGS_PERMITTED` retains CF, PF, AF, ZF, SF, TF, DF, OF, ID; `USER_RFLAGS_FORCED` sets the MBO bit 1 + IF (bit 9). All other bits — IOPL, NT, RF, VM, AC, VIF, VIP, plus the entire reserved high half — are masked to zero. Eight lib tests verify each axis of the masking (`set_rflags_clears_iopl`, `…_clears_ac`, `…_clears_nt_and_vm`, `…_clears_vif_vip_and_rf`, `…_forces_if_and_mbo`, `…_preserves_id_and_user_arith_flags`, `…_drops_all_other_bits`, `cs_ss_forced_to_user_selectors`).)*
- [x] **1G.3** In `slopos-ostd/src/user/mode.rs`, define:
  ```rust
  pub struct UserMode<'a> {
      ctx: &'a mut UserContext,
      space: &'a VmSpace,
  }
  pub enum ReturnReason {
      Syscall(u64),         // syscall number in rax
      Exception(ExceptionInfo),
      Interrupt(u8),        // vector
  }
  impl<'a> UserMode<'a> {
      pub fn new(ctx: &'a mut UserContext, space: &'a VmSpace) -> Self;
      pub fn execute(self) -> ReturnReason;
  }
  ```
  *(Done — `slopos-ostd/src/user/mode.rs`. `ReturnReason` carries a `ExceptionInfo { vector, error_code, fault_addr }` payload (CR2 propagates via `fault_addr` for `#PF`, else 0). `UserMode::new` records the borrow; `execute(self)` consumes the wrapper so a single round trip is enforced by the type system. `ctx()` / `ctx_mut()` accessors expose the carried `&mut UserContext` for pre-flight tweaks (e.g. signal-frame setup before the IRETQ).)*
- [x] **1G.4** `UserMode::execute` ports today's user-mode entry path (currently spread across `core/src/scheduler/switch_asm.rs` and syscall entry stubs). All inline asm goes here. SAFETY refs Inv. 2. *(Done — the asm split is: (a) a `#[cfg(target_arch = "x86_64")] core::arch::global_asm!` defining the `__ostd_user_return` re-entry trampoline whose body is intentionally `ud2`-shaped — accidental routing into it before 1J wires the IDT triggers `#UD` with RIP pointing at the labelled symbol, which is exactly the diagnostic we want from a "do not invoke yet" stub; (b) `user_return_trampoline_addr()` exposes the symbol's address to the IDT installer in `boot/`. The forward direction (kernel → user) flows through a `UserModeBackend` trait — `unsafe trait UserModeBackend { unsafe fn execute_round_trip(&self, ctx_ptr: *mut UserContext, space: &VmSpace) -> ReturnReason; }` — registered via the same one-shot pattern 1F established for `PreemptBackend` (`UnsafeCell<MaybeUninit<&'static dyn …>>` + `AtomicBool::swap(true, AcqRel)`). Default `PanicBackend` panics with a "Phase 1J wiring TODO" message; this is the mechanism that lets `UserMode::execute` link without 1J's per-CPU PCR slot existing yet. **No kernel call site invokes `UserMode::execute` in 1G.** SAFETY comments on `execute_round_trip` and the trampoline asm reference Inv. 2 (kernel-mode CPU state untamperable) and Inv. 5 (user pointers cannot reach kernel memory — IRETQ enforces canonicality).)*
- [x] **1G.5** In `slopos-ostd/src/user/copy.rs`, port `mm/src/user_copy.rs::raw_usercopy` (assembly + RIP-rewrite fault recovery). Provide:
  ```rust
  pub fn copy_from_user<T: Pod>(space: &VmSpace, src: UserPtr<T>) -> Result<T, UserCopyError>;
  pub fn copy_to_user<T: Pod>(space: &VmSpace, dst: UserPtr<T>, value: &T) -> Result<(), UserCopyError>;
  pub fn copy_bytes_from_user(space: &VmSpace, src: Vaddr, dst: &mut [u8]) -> Result<(), UserCopyError>;
  pub fn copy_bytes_to_user(space: &VmSpace, dst: Vaddr, src: &[u8]) -> Result<(), UserCopyError>;
  ```
  Note `copy_*_from_user` returns `T` by value (uses `MaybeUninit` internally); never a `&T`. *(Done — `slopos-ostd/src/user/copy.rs`. Asm symbols `__ostd_raw_usercopy` / `__ostd_usercopy_start` / `__ostd_usercopy_end` / `__ostd_usercopy_fault` are deliberately distinct from the legacy `mm/src/user_copy.rs::raw_usercopy` symbols so both implementations link side-by-side until 1J retires the legacy path. Page-table reachability is checked through `VmSpace::cursor` (1D) — the new shape replaces `mm::user_copy::validate_user_pages`'s direct `paging_is_user_accessible` walk and removes the `current_process_dir()` thread-local lookup. `UserCopyError` carries `NotMapped | NotUserAccessible | NotUserWritable | OutOfUserRange | Fault { bytes_copied } | InvalidSpace` — finer than the legacy `UserPtrError::CopyFailed`. Public `is_ostd_usercopy_ip(rip)` / `ostd_usercopy_fault_ip()` mirror the legacy `is_usercopy_ip` / `usercopy_fault_ip` so 1J's page-fault handler can query the OSTD range alongside the legacy one. The signatures take `UserVirtAddr` (not raw `Vaddr`) for `copy_bytes_*` — this tightens the call surface vs. the plan-template signature: a `UserVirtAddr` is constructible only inside OSTD (private `try_new`), so any caller that wants to invoke `copy_bytes_*` must either own a `UserContext` (giving `user_bytes_arg`) or hold a `UserPtr` and call `.addr()`. No raw `u64` user-base-addr leak through the signature. The compile-fail doctest below (`copy_from_user.rs:174`) confirms `let _: &u64 = copy_from_user(...)` is rejected by `T: Pod` returning by value.)*
- [x] **1G.6** `UserPtr<T>`: re-export from `mm/src/user_ptr.rs`, but make construction private to OSTD. The only way to get a `UserPtr` is via `UserContext::user_ptr_arg::<T>(reg_index)`. Ensures Inv. 5 (user-supplied addresses can't access kernel memory). *(Done — canonical type now lives at `slopos-ostd/src/user/ptr.rs`. `UserVirtAddr` / `UserPtr<T>` / `UserSlice<T>` / `UserBytes` constructors are `pub(crate)`; the only public construction surfaces are `UserContext::user_ptr_arg::<T>(reg_index)` / `user_slice_arg::<T>(base_idx, count_idx)` / `user_bytes_arg(base_idx, len_idx)`. Register indices map to the Linux x86_64 syscall ABI: `0 → rdi`, `1 → rsi`, `2 → rdx`, `3 → r10`, `4 → r8`, `5 → r9`. Out-of-range indices panic (programmer error). `UserPtrError` carries `Null | NonCanonical | OutOfUserRange | Overflow`. The validation order is: null-check → canonical-check → user-range-check → length-overflow-check; the lib tests pin every ordering edge so future refactors cannot silently swap the precedence (`null_rejected`, `non_canonical_rejected`, `kernel_half_canonical_but_out_of_user_range`, `at_user_end_rejected`, `straddles_user_end_overflow`, `happy_path`, `user_ptr_carries_size`, `user_slice_overflow_in_count_mul`). `mm/src/user_ptr.rs` is **left untouched** — its public `UserPtr::try_new` continues to serve every existing kernel call site. Phase 1J replaces those callers with `ctx.user_ptr_arg::<T>(reg_index)` and then deletes the legacy file in favour of a thin re-export.)*
- [x] **1G.7** SMAP discipline: `stac` / `clac` happen inside `slopos-ostd::user::copy`, never exposed. Inv. 4. *(Done — the only `STAC` / `CLAC` instructions in OSTD live inside the four-line `__ostd_raw_usercopy` `global_asm!` block in `slopos-ostd/src/user/copy.rs`. There is no `pub fn stac()` / `pub fn clac()` anywhere in the crate; verified via `grep -rn 'stac\|clac' slopos-ostd/src` returning the four asm lines only. The user-side leak is closed in `UserContext::set_rflags` from 1G.2 — AC (bit 18) is in neither the permitted nor the forced mask, so any user RFLAGS that flows through `set_rflags` lands with AC = 0 in the IRETQ frame.)*
- [x] **1G.8** Verify: `cargo check -p slopos-ostd` succeeds. Compile-fail test: `let _: &T = copy_from_user(...)` fails. *(Done — `cargo check -p slopos-ostd` and `cargo check -p slopos-ostd-derive` clean. `cargo test -p slopos-ostd` reports **96 lib + 9 dma + 11 io_mem + 6 io_port + 10 irq_line + 10 preempt + 7 uframe + 9 user_mode + 12 vm_space + 7 doctest = 177 passes, 0 failures**, up from 140 pre-1G (+37). The new doctest at `slopos-ostd/src/user/copy.rs:174` is the `compile_fail`-typed `let _: &u64 = copy_from_user(...)` from the spec. `cargo fmt --all -- --check` clean. `just build` finishes in ~4.3 s with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no new ≥ 2 KiB frames; the deepest `MaybeUninit<T>` slot in `copy_from_user` is bounded by the `T: Pod` size — primitives + arrays only, well under 2 KiB). The kernel's existing user-entry/exit path (`boot/idt_handlers.s`, `core/context_switch.s`, `mm/src/user_copy.rs`, `mm/src/user_ptr.rs`) compiles and runs unchanged — the new OSTD primitives are unwired, so `just boot` is byte-identical to pre-1G. **TCB delta**: 19 `unsafe` tokens added across the four new files: `context.rs` (3 — `unsafe impl Send for FpuStateRef`, `unsafe impl Sync for FpuStateRef`, `unsafe fn from_raw` decl); `copy.rs` (6 — three `unsafe { __ostd_raw_usercopy(...) }` blocks, one `unsafe { dst.assume_init() }`, two `extern "C"` decls counting `unsafe extern "C"`); `mode.rs` (10 — `unsafe trait UserModeBackend`, `unsafe fn execute_round_trip` trait method decl, `unsafe impl UserModeBackend for PanicBackend`, `unsafe impl Sync for BackendSlot`, `unsafe fn register_user_mode_backend`, the matching MaybeUninit::write/assume_init blocks, the `unsafe extern "C"` block for the trampoline import, and the `unsafe { backend.execute_round_trip(...) }` call site); `ptr.rs` (0 — pure validation, no asm or pointer manipulation). Every `unsafe` block carries a `// SAFETY:` comment naming Inv. 2 (kernel-mode CPU state), Inv. 4 (SMAP discipline), or Inv. 5 (user pointers cannot reach kernel memory). **Phase 1J wiring TODO**: (1) page-fault handler in `boot/src/exception.rs::exception_page_fault` queries `slopos_ostd::user::copy::is_ostd_usercopy_ip` alongside the legacy `mm::user_copy::is_usercopy_ip` and redirects RIP to whichever recovery point matches; (2) IDT vectors for syscall + exception + external-interrupt edges from user mode route through `slopos_ostd::user::mode::user_return_trampoline_addr()` instead of the legacy `boot/idt_handlers.s` IRETQ stubs; (3) syscall MSR `LSTAR` points at `__ostd_user_return`; (4) `register_user_mode_backend` wired to a per-CPU PCR slot that stashes the active `*mut UserContext` and restores callee-saved kernel registers across the round trip; (5) consumer migration of every `mm::user_copy::*` call site to `slopos_ostd::user::copy::*` (call surface tightening: legacy takes `T: Copy`, OSTD takes `T: Pod`); (6) consumer migration of every `mm::user_ptr::*::try_new` call site to `ctx.user_ptr_arg::<T>(reg_index)` / `ctx.user_slice_arg::<T>(base_idx, count_idx)` / `ctx.user_bytes_arg(base_idx, len_idx)`; (7) deletion of `mm/src/user_copy.rs` + `mm/src/user_ptr.rs` after the migration is complete; (8) replacement of `slopos_arch::InterruptFrame` (which the legacy syscall dispatch uses today) with `slopos_ostd::user::context::UserContext` so syscall handlers consume the OSTD-canonical type. None of (1)–(8) was performed in 1G itself — every existing `mm::user_copy::*` / `mm::user_ptr::*` / `boot/idt_handlers.s` / `core/context_switch.s` caller continues to use the legacy types unchanged.)*

### 1H: Allocation surface — fold `slopos-alloc` into OSTD

`slopos-alloc` becomes `slopos-ostd::mm::heap` and `slopos-ostd::mm::init`. Kernel API surface (`KBox`, `KVec`, `KArc`, `PinBox`, `Init<T,E>`, `Zeroable`) is preserved.

- [ ] **1H.1** Move `slopos-alloc/src/lib.rs` → `slopos-ostd/src/mm/heap.rs`. Re-export at crate root: `slopos_ostd::KBox`, `slopos_ostd::KVec`, etc.
- [ ] **1H.2** Move `slopos-alloc/src/init.rs` → `slopos-ostd/src/mm/init.rs`. Re-export `Init`, `Zeroable`, `init_from_closure`, `init_zeroed`.
- [ ] **1H.3** The `#[global_allocator]` and `#[alloc_error_handler]` declarations stay in `kernel/src/main.rs` (CLAUDE.md exception preserved). They now reference the `slopos_ostd::mm::heap::KernelHeap` type.
- [ ] **1H.4** Define `pub trait FrameAlloc` and `pub trait Slab` in `slopos-ostd::mm`:
  ```rust
  pub trait FrameAlloc: Send + Sync {
      fn alloc(&self, layout: Layout) -> Option<Paddr>;
      fn dealloc(&self, addr: Paddr, layout: Layout);
      fn add_free_memory(&self, addr: Paddr, size: usize);
  }
  pub trait Slab: Send + Sync {
      type Slot;
      fn alloc(&self) -> Option<Self::Slot>;
      fn dealloc(&self, slot: Self::Slot);
  }
  ```
  Phase-1 ships an internal default impl that wraps today's `mm/src/page_alloc.rs` allocator. Phase 2 replaces this with a safe-Rust impl outside OSTD.
- [ ] **1H.5** Delete `slopos-alloc/` directory. Update workspace `Cargo.toml`. Update `scripts/check_alloc_dep.sh` to look for `slopos_ostd::mm::heap` paths instead of `slopos_alloc`.
- [ ] **1H.6** `KBox::try_init(Init<T,E>)` discipline preserved verbatim. Stack-frame ceiling (2 KiB via `scripts/check_stack_sizes.sh`) preserved verbatim.
- [ ] **1H.7** Verify: `just build` succeeds. `cargo fmt --all` clean. `just test` runs with the same test count as pre-1H.

### 1I: Sync primitives + Task primitive

Move `sync/` into `slopos-ostd::sync`. Define low-level `Task` (NOT async — that's Phase 3).

- [ ] **1I.1** Move every file in `sync/src/` into `slopos-ostd/src/sync/`:
  - `spinlock.rs` → `sync/spin.rs` (rename `IrqMutex` → `SpinLock`, ticket-lock impl preserved).
  - `cpu_local.rs` → `sync/cpu_local.rs`.
  - `preempt.rs` → `cpu/preempt.rs`.
  - `rcu.rs` → `sync/rcu.rs`.
  - `seqlock.rs` → `sync/seqlock.rs`.
  - `waitqueue.rs` → `sync/wait_queue.rs`.
  - `init_flag.rs` → `sync/init_flag.rs`.
  - `once_lock.rs` → `sync/once_lock.rs`.
  - `lock_tracking.rs` → `sync/lock_tracking.rs`.
- [ ] **1I.2** Define `slopos-ostd::sync::Mutex<T>` (sleeping mutex, distinct from `SpinLock`). Internally uses `WaitQueue`.
- [ ] **1I.3** Delete `sync/` crate directory. Update workspace `Cargo.toml`. Update consumers (`s/sync::/slopos_ostd::sync::/g` in non-OSTD crates).
- [ ] **1I.4** In `slopos-ostd/src/task/task.rs`, define the bare `Task` primitive:
  ```rust
  pub struct Task {
      id: TaskId,
      generation: u64,           // generation-counter handle
      kernel_stack: KernelStack,
      ctx: TaskContext,
      vm_space: Option<KArc<VmSpace>>,
      fpu_state: FpuState,
      is_running: AtomicBool,    // Inv. 8
      // ... no async state yet
  }
  pub struct TaskContext {
      // saved callee-saved regs, RSP, RFLAGS, RIP
  }
  pub struct CurrentTask { /* token: this CPU's current task */ }
  pub fn current() -> CurrentTask;
  ```
- [ ] **1I.5** Define `pub trait Scheduler` and `pub trait RunQueue`:
  ```rust
  pub trait Scheduler: Send + Sync {
      fn enqueue(&self, task: TaskRef);
      fn local_rq_with(&self, f: &mut dyn FnMut(&mut dyn RunQueue));
  }
  pub trait RunQueue {
      fn update_curr(&mut self);
      fn pick_next(&mut self) -> Option<TaskRef>;
      fn dequeue_curr(&mut self) -> Option<TaskRef>;
  }
  ```
  Phase 1 provides a default `RoundRobinScheduler` impl inside OSTD (later moved out in Phase 2).
- [ ] **1I.6** In `slopos-ostd/src/task/switch.rs`, port `core/src/scheduler/switch_asm.rs` (context switch, task entry trampoline, init-current-context). Keep naked-fn implementations. SAFETY refs Inv. 8.
- [ ] **1I.7** In `slopos-ostd/src/task/fpu.rs`, port FPU XSAVE/XRSTOR (today in `core/context_switch.s` + `core/src/scheduler/switch_asm.rs:185`). 64-byte alignment enforced via `#[repr(C, align(64))]` on `FpuState`.
- [ ] **1I.8** `Task` Drop tears down kernel stack, drops VmSpace ref, frees FPU state slab slot. Inv. 9.
- [ ] **1I.9** Verify: `cargo check -p slopos-ostd` succeeds. `just build` succeeds (consumers still on FFI shims).

### 1J: Migrate existing kernel to consume OSTD (parity)

This subtask is the bulk of Phase 1's clock time. Every existing kernel crate is rewritten to consume OSTD instead of its own internals. Behavior must be identical.

- [ ] **1J.1** `karch/`: replace its `lib.rs` with re-exports from `slopos_ostd::cpu::x86_64`. Delete crate-internal CPU HAL files (`arch/`, `cpu/`, `init_flag.rs`, `interrupt_frame.rs`, `pcr.rs`, `tsc.rs`).
- [ ] **1J.2** `boot/`: replace `boot/src/idt.rs` IDT setup with calls to `slopos_ostd::irq::idt::install`. Replace `boot/src/gdt.rs` with `slopos_ostd::arch::x86_64::gdt`. Delete duplicated entries.
- [ ] **1J.3** `mm/src/mmio.rs`: replace `MmioRegion` with a type alias `pub type MmioRegion = slopos_ostd::IoMem;`. Eventually delete the alias in Phase 2.
- [ ] **1J.4** `mm/src/page_alloc.rs`: replace `OwnedPageFrame` with a type alias to `Frame<KernelMeta>`. Internal `unsafe` blocks now reference Inv. 1 + Inv. 4.
- [ ] **1J.5** `mm/src/process_vm.rs::ProcessVmInner`: hide raw `pml4` pointer. Public field becomes `vm_space: KArc<VmSpace>`. All internal mutation via `vm_space.cursor_mut(..)`.
- [ ] **1J.6** `mm/src/paging/`: most of this code becomes private to `slopos-ostd::mm::vm_space`. Delete `paging/tables.rs::ProcessPageDir` (replaced by `VmSpace`). Delete `paging/tables.rs::split_pdpt_huge` etc. (now private inside OSTD).
- [ ] **1J.7** `mm/src/user_copy.rs`: becomes a thin re-export of `slopos_ostd::user::copy`. Delete `raw_usercopy` (now in OSTD). Delete `UserPtr` (re-exported from OSTD).
- [ ] **1J.8** `core/src/scheduler/switch_asm.rs`: delete; functionality now in `slopos_ostd::task::switch`.
- [ ] **1J.9** `core/src/scheduler/`: rewrite to consume `slopos_ostd::task::Task`, `slopos_ostd::task::Scheduler` (the trait). At Phase 1 end, the scheduler still has whatever logic it has today — just consuming OSTD primitives.
- [ ] **1J.10** `core/src/syscall/`: handlers continue to receive raw frames at this phase. Phase 2 redesigns the dispatch. However, `syscall::context::SyscallContext` migrates to taking `&mut UserContext` instead of raw frame pointer.
- [ ] **1J.11** `core/src/irq.rs`: thin wrapper re-exporting `slopos_ostd::irq` for legacy callers. Delete in Phase 2.
- [ ] **1J.12** `drivers/`: every driver that uses `MmioRegion` already works (alias in 1J.3). Drivers using `port_in*`/`port_out*` migrate to `IoPort<T>`. Drivers registering IRQs migrate to `IrqLine::register_callback`.
- [ ] **1J.13** `fs/`, `net/`, `acpi/`: chase compile errors from the renames above. No semantic changes.
- [ ] **1J.14** Crucially: at the end of 1J, every kernel crate **except `slopos-ostd`** must have *zero* `unsafe` blocks. This will require some compile errors to be fixed by introducing safe OSTD APIs — that's expected. Track them as 1J.14.{a..z} sub-items.
- [ ] **1J.15** Run `just test`. Test count must equal pre-1J. Any test failure is a 1J defect.
- [ ] **1J.16** Verify: `rg 'unsafe' --type rust -g '!slopos-ostd/**'` returns zero matches in kernel crates. (Userland excluded.)

### 1K: KernMiri port

Dynamic UB detection on OSTD. Asterinas's KernMiri is +1,200 LoC. Port the concepts; we don't need to fork Miri ourselves on day one.

- [ ] **1K.1** Add `tools/kernmiri/` directory. README explains what KernMiri is and links to Asterinas's fork.
- [ ] **1K.2** Decide between forking Miri (Asterinas approach) and using stock Miri with a host-side simulation harness (cheaper). **Recommendation**: stock Miri + a `cfg(miri)` feature on `slopos-ostd` that swaps real-hardware ops for fake ones. Document the choice in `tools/kernmiri/README.md`.
- [ ] **1K.3** Write Miri shims for:
  - Physical memory simulation (a `Vec<u8>` backing store; `Frame::from_unused` allocates from it).
  - Page table simulation (in-memory tree, no real CR3 writes).
  - IRQ simulation (deterministic delivery for determinism).
- [ ] **1K.4** Port the `slopos-ostd::mm::frame::tests` to run under Miri (`cargo +nightly miri test -p slopos-ostd --features miri`).
- [ ] **1K.5** Port the `slopos-ostd::mm::vm_space::tests` to run under Miri.
- [ ] **1K.6** Port the `slopos-ostd::sync` tests (spinlock, RCU, wait queue) to run under Miri.
- [ ] **1K.7** Add `just check-miri` recipe. Wire into CI.
- [ ] **1K.8** **Coverage gate**: `slopos_ostd::mm` and `slopos_ostd::sync` must hit ≥90% line coverage under Miri before Phase 1 closes.
- [ ] **1K.9** Document any UBs found and the fixes in `slopos-ostd/MIRI_FINDINGS.md`.

### 1L: Build gates

Make the framekernel discipline load-bearing in CI.

- [ ] **1L.1** Replace `scripts/check_alloc_dep.sh` (which checks `extern crate alloc`) with `scripts/check_unsafe_outside_ostd.sh`:
  - Greps every `.rs` under kernel crates for `\bunsafe\b` (with cfg-gated lookback).
  - Skips `slopos-ostd/`, `kernel/src/main.rs` (global_allocator declaration).
  - Fails build on any match.
- [ ] **1L.2** Update `scripts/check_alloc_dep.sh` (still useful) to also catch `use ::alloc::` inside non-OSTD crates.
- [ ] **1L.3** `scripts/check_stack_sizes.sh` keeps its 2 KiB ceiling. Add a comment that this is Inv. 5'.
- [ ] **1L.4** Add `scripts/tcb_ratio.sh`: counts `unsafe`-tagged tokens in `slopos-ostd/`, divides by total kernel LoC, prints percent. Wire into PR template via a CI comment bot (or just a `just tcb-ratio` recipe for now).
- [ ] **1L.5** Add `just check-framekernel` recipe that runs all of: `check_unsafe_outside_ostd.sh`, `check_alloc_dep.sh`, `check_stack_sizes.sh`, `cargo fmt --all -- --check`, `cargo clippy -- -D warnings`, `just check-miri`.
- [ ] **1L.6** Update `CLAUDE.md` "Allocation surface" section: replace with "Unsafe-code surface — `slopos-ostd` is the only kernel crate allowed to use `unsafe`. Build gate `scripts/check_unsafe_outside_ostd.sh` enforces this." Keep stack-size and KBox::try_init prose.
- [ ] **1L.7** Add `#![forbid(unsafe_code)]` to every non-OSTD kernel crate's `lib.rs`. Verify nothing compiles unsafely.

### 1M: Phase 1 close

- [ ] **1M.1** Run `just check-framekernel`. Zero failures.
- [ ] **1M.2** Run `just test`. Full pass; test count equal to or greater than pre-Phase-1.
- [ ] **1M.3** Compute TCB ratio with `just tcb-ratio`. Confirm ≤1.5%.
- [ ] **1M.4** Run LMbench equivalent (`tools/run_tests/` perf subset, or hand-write one). Confirm ±5% vs. pre-Phase-1.
- [ ] **1M.5** Run KernMiri suite (`just check-miri`). Confirm ≥90% line coverage on `slopos_ostd::mm` and `slopos_ostd::sync`. Zero UBs.
- [ ] **1M.6** Update `plans/README.md` with FRAMEKERNEL_PLAN.md entry.
- [ ] **1M.7** Tag the commit `framekernel-phase-1`. Open a Phase-1 close PR with TCB ratio, test summary, perf delta.
- [ ] **1M.8** Update this plan: mark all Phase-1 boxes checked; status in front-matter to `phase-2-ready`.

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
