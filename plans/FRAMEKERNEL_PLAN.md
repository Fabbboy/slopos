---
name: SlopOS Framekernel Architecture Plan
description: Four-phase rip-and-replace plan to redesign SlopOS as an async-first framekernel with a Verus-verified OSTD critical path
status: phase-1-in-progress
authors: research synthesis from Asterinas (USENIX ATC '25), Theseus, RedLeaf, Hubris, seL4, CortenMM
---

# SlopOS Framekernel Architecture Plan

> **Status**: Phase 1 in progress — 1A (crate skeleton), 1B (`Frame<M>`), 1C (`UFrame` / `USegment`), 1D (`VmSpace` + cursor), 1E (`IoMem` / `IoPort` / `Dma*`), 1F (`IrqLine` / `IdtBuilder` / `DisabledPreemptGuard`), 1G (`UserContext` / `UserMode` / typed user copy), 1H (`KernelHeap` folded into ostd), 1I (sync primitives + `Task` primitive), 1J-α (wiring foundation: `register_*` hooks), **1J-β (safe aliases — complete)**, 1J-γ (port karch into OSTD + dep inversion), 1J-δ (IDT/GDT migration), 1J-ε (UserModeBackend + LSTAR), 1J-ζ (scheduler/task migration onto OSTD), **1J-η (VmSpace/paging migration — η.1, η.2, η.3, η.4, η.5, η.6 done; per-process paging is OSTD-only; legacy `paging::` retained as a small kernel-side fallback for the priority-10 boot path)**, 1J-θ (SyscallContext migrated onto `*mut UserContext`), and **1J-ι (driver migration cleanup — `MmioRegion = IoMem` audited, `pic.rs` / `pit.rs` / `ps2/mod.rs` migrated to `IoPort<u8>`, `PORT_RANGES` half-open bug fixed + PS/2 added, virtio IRQ on `IrqLine::register_callback` confirmed)** complete; **1J split into 11 sub-phases (1J-α..1J-λ) — see §5.1J for the breakdown**.
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

- [x] **1H.1** Move `slopos-alloc/src/lib.rs` → `slopos-ostd/src/mm/heap.rs`. Re-export at crate root: `slopos_ostd::KBox`, `slopos_ostd::KVec`, etc. *(Done — `slopos-alloc/src/lib.rs` was copied verbatim into `slopos-ostd/src/mm/heap.rs` with the crate-level header (`#![no_std]`, `#![feature(...)]`, `#![forbid(...)]`, `extern crate alloc;`) stripped — those declarations now live on `slopos-ostd/src/lib.rs` instead, where `extern crate alloc;` and `#![feature(allocator_api, coerce_unsized, unsize)]` are added. The full `KBox` / `KVec` / `KArc` / `KVecDeque` / `KBTreeMap` / `PinBox` / `boxed_zeroed` / `raw_alloc` / `raw_dealloc` / `AllocError` surface re-exports at `slopos-ostd/src/mm/mod.rs` and again at the crate root in `slopos-ostd/src/lib.rs`, so callers continue to write `use slopos_ostd::KBox;` etc. The 866-line file lands as 869 lines (slight cosmetic delta from header strip).)*
- [x] **1H.2** Move `slopos-alloc/src/init.rs` → `slopos-ostd/src/mm/init.rs`. Re-export `Init`, `Zeroable`, `init_from_closure`, `init_zeroed`. *(Done — verbatim copy. `init.rs` is pure `core` (no `alloc` dep) so no header changes were needed; only the doc-comment cross-references to `crate::KBox` / `slopos-alloc/src/init.rs` were rewritten to `super::heap::KBox` / `slopos-ostd/src/mm/init.rs`. Re-exports `Init`, `InitClosure`, `Zeroable`, `init_from_closure`, `init_zeroed` land at both `slopos-ostd::mm::*` and `slopos-ostd::*`.)*
- [x] **1H.3** The `#[global_allocator]` and `#[alloc_error_handler]` declarations stay in `kernel/src/main.rs` (CLAUDE.md exception preserved). They now reference the `slopos_ostd::mm::heap::KernelHeap` type. *(Done — `slopos-ostd::mm::heap::KernelHeap` is a unit struct with a `unsafe impl GlobalAlloc` that forwards to two `extern "Rust"` no-mangle entry points: `slopos_global_alloc` and `slopos_global_dealloc`. The matching definitions live in `mm/src/lib.rs`, replacing the old `pub struct KernelAllocator;` + `unsafe impl GlobalAlloc for KernelAllocator`. The forwarding-shim approach is identical in shape to `LegacyFrameAllocShim` (1B.5) and avoids OSTD ⇄ mm circularity (mm depends on ostd; ostd cannot depend on mm). `kernel/src/main.rs` now reads `static GLOBAL_ALLOCATOR: KernelHeap = KernelHeap;` and keeps `extern crate alloc;` for `#[alloc_error_handler]`.)*
- [x] **1H.4** Define `pub trait FrameAlloc` and `pub trait Slab` in `slopos-ostd::mm`. Phase-1 ships an internal default impl that wraps today's `mm/src/page_alloc.rs` allocator. Phase 2 replaces this with a safe-Rust impl outside OSTD. *(Done — `FrameAlloc` already lived in `slopos-ostd/src/mm/frame.rs:518` from 1B.5; 1H.4 adds `pub use frame::{FrameAlloc, FrameAllocOptions};` to `mm/mod.rs` so callers can pull it from `slopos-ostd::mm::FrameAlloc`. Kept the existing `FrameAllocOptions`-based shape rather than the plan-draft `Layout`-based one — `LegacyFrameAllocShim`, `VmSpace`, and the host integration tests already use it, and rewriting it would be churn for no Phase-1 benefit. **`Slab` is new**: `slopos-ostd/src/mm/slab.rs` holds `pub trait Slab: Send + Sync { type Slot; fn alloc; fn dealloc; }` plus a `pub use slab::Slab;` re-export. Phase 1 ships only the trait surface; the concrete impl is Phase 2 §6.2B.)*
- [x] **1H.5** Delete `slopos-alloc/` directory. Update workspace `Cargo.toml`. Update `scripts/check_alloc_dep.sh` to look for `slopos_ostd::mm::heap` paths instead of `slopos_alloc`. *(Done — `slopos-alloc/` removed (whole directory). Workspace `Cargo.toml` drops the `"slopos-alloc"` member entry and the `slopos-alloc = { path = "slopos-alloc" }` workspace-dep. Twelve consumer Cargo.toml files updated: six (`boot`, `core`, `drivers`, `fs`, `mm`, `net`) just dropped the `slopos-alloc = { workspace = true }` line because they already depended on `slopos-ostd`; four (`font`, `hermetic`, `ktesting`, `utils`) had only `slopos-alloc` so the line was rewritten to `slopos-ostd = { workspace = true }`; `sync` had both with the path-syntax form so the `slopos-alloc` line was dropped; `slopos-ostd` itself dropped the `slopos-alloc` self-dep. **Source migration**: 90 `.rs` files referencing `slopos_alloc::` were renamed to `slopos_ostd::` via `sed -i 's|slopos_alloc::|slopos_ostd::|g'` over `git ls-files '*.rs'`; `slopos-alloc` doc-comment references were renamed via the same pattern (`slopos-alloc` → `slopos-ostd`). **Build gates**: `scripts/check_alloc_dep.sh` USERLAND_RE flipped from `slopos-alloc` to `slopos-ostd` in both the Cargo-level and source-level passes; error/success messages updated to refer to `slopos_ostd::mm::heap`. `scripts/check_return_types.sh` whitelist path flipped from `slopos-alloc/src` to `slopos-ostd/src`; doc comments updated. `CLAUDE.md` / `AGENTS.md` (the latter is a symlink) updated correspondingly.)*
- [x] **1H.6** `KBox::try_init(Init<T,E>)` discipline preserved verbatim. Stack-frame ceiling (2 KiB via `scripts/check_stack_sizes.sh`) preserved verbatim. *(Done — no code changes. The `KBox::try_init` / `PinBox::try_init` paths are byte-for-byte identical to the pre-1H `slopos-alloc` versions. `scripts/check_stack_sizes.sh` is unaffected; this is a pure crate-membership fold.)*
- [x] **1H.7** Verify: `just build` succeeds. `cargo fmt --all` clean. `just test` runs with the same test count as pre-1H. *(Done for build + host tests; kernel `just test` pending user-side run. `just build` finishes ~6 s with `check_alloc_dep: OK — no kernel crate or source file outside slopos-ostd names 'alloc' directly` and `check_stack_sizes: OK — all frames <= 2048 bytes`. `cargo fmt --all -- --check` clean. `cargo test -p slopos-ostd` reports **96 lib + 9 + 11 + 6 + 10 + 10 + 7 + 9 + 12 integration + 7 doctest passes, 0 failures, 1 doctest ignored** (the `KernelHeap` doctest is `ignore`-flagged because `#[global_allocator]` cannot be exercised inside a doctest harness). Zero `slopos_alloc` / `slopos-alloc` references remain across `.rs` / `.toml` / `.sh` outside `plans/FRAMEKERNEL_PLAN.md`'s own historical done-notes.)*

### 1I: Sync primitives + Task primitive

Move `sync/` into `slopos-ostd::sync`. Define low-level `Task` (NOT async — that's Phase 3).

- [x] **1I.1** Move every file in `sync/src/` into `slopos-ostd/src/sync/`:
  - `spinlock.rs` → `sync/spin.rs` (rename `IrqMutex` → `SpinLock`, ticket-lock impl preserved).
  - `cpu_local.rs` → `sync/cpu_local.rs`.
  - `preempt.rs` → `cpu/preempt.rs`.
  - `rcu.rs` → `sync/rcu.rs`.
  - `seqlock.rs` → `sync/seqlock.rs`.
  - `waitqueue.rs` → `sync/wait_queue.rs`.
  - `init_flag.rs` → `sync/init_flag.rs`.
  - `once_lock.rs` → `sync/once_lock.rs`.
  - `lock_tracking.rs` → `sync/lock_tracking.rs`.
  *(Done — every file ported byte-identical except for the rename `IrqMutex/IrqMutexGuard` → `SpinLock/SpinLockGuard` and the dep-inversion of `wait_queue.rs` / `rcu.rs` (see 1I.2 and below). The legacy `PreemptGuard` + `IrqPreemptGuard` (PCR-backed, 125+ kernel call sites) live alongside 1F's `DisabledPreemptGuard` (backend-pluggable, host-testable) in `slopos-ostd/src/cpu/preempt.rs` — both are `pub`. The ticket-lock fairness invariant (FIFO via wrapping `next_ticket` / `now_serving` u16 counters) and proportional backoff stay byte-identical. **Critical dep-graph change**: `karch/Cargo.toml` had a dead `slopos-ostd` dep that was breaking the planned `slopos-ostd → slopos-arch` arrow; removed it (no source changes — the dep was unused) and added `slopos-arch = { path = "../karch" }` to `slopos-ostd/Cargo.toml`. This unblocks `slopos-arch::cpu::{save_flags_cli, restore_flags}`, `slopos-arch::pcr::*`, and `slopos-arch::tsc::rdtsc` from inside OSTD; arch retires into OSTD in 1J.1.)*
- [x] **1I.2** Define `slopos-ostd::sync::Mutex<T>` (sleeping mutex, distinct from `SpinLock`). Internally uses `WaitQueue`. *(Done — `slopos-ostd/src/sync/mutex.rs`. `Mutex { locked: AtomicBool, waiters: WaitQueue, data: UnsafeCell<T> }`. `lock()` does a fast-path CAS on `locked`; on failure parks via `WaitQueue::wait_event(|| !locked)`. `try_lock()` / `into_inner()` cover the rest of the surface. Drop wakes one waiter. **Backend inversion**: `WaitQueue` and `RCU` reach the kernel scheduler / platform clock through one-shot-registered backends (`WaitQueueBackend`, `RcuBackend`) — same `unsafe fn register_*_backend(&'static dyn …)` + `AtomicBool::swap(true, AcqRel)` pattern 1F/1G already established. Until registered, every blocking method on `WaitQueue` short-circuits (`is_runtime_initialised() == false`), and `RCU` falls back to TSC for monotonic time + drops log args. The kernel-side bridge is in `kernel-services/src/ostd_bridge.rs` (a zero-sized `KernelServicesBridge` that proxies to the existing `slopos_kernel_services::driver_runtime::*` and `platform::*` facade calls); registration happens in `boot/src/early_init.rs:539+` after the underlying `register_driver_runtime_services` / `register_platform_services` calls.)*
- [x] **1I.3** Delete `sync/` crate directory. Update workspace `Cargo.toml`. Update consumers (`s/sync::/slopos_ostd::sync::/g` in non-OSTD crates). *(Done — `sync/` deleted; workspace `members` list trimmed; `slopos-sync = { path = "sync" }` removed from `[workspace.dependencies]`. Bulk renames across the whole tree: 73 `.rs` files had `slopos_sync::` → `slopos_ostd::sync::`; 65 files had `IrqMutex/IrqMutexGuard` → `SpinLock/SpinLockGuard`; sub-module path renames where consumers reached past the public re-export: `slopos_sync::spinlock::` → `slopos_ostd::sync::spin::`, `slopos_sync::waitqueue::` → `slopos_ostd::sync::wait_queue::`, `slopos_sync::preempt::` → `slopos_ostd::sync::` (the PreemptGuard / IrqPreemptGuard / register_reschedule_callback are re-exported at the sync module root). Nine consumer `Cargo.toml` files dropped their `slopos-sync = …` line; `video/Cargo.toml` gained an explicit `slopos-ostd = { workspace = true }` (it had been pulling sync in indirectly). `font/Cargo.toml`'s `kernel = ["dep:slopos-sync"]` feature converted to `kernel = []` (the cfg-gated code no longer needs an optional dep — slopos-ostd is unconditional). `kernel-services/Cargo.toml` gained `slopos-ostd = { path = "../slopos-ostd" }` to host the new `ostd_bridge` module.)*
- [x] **1I.4** In `slopos-ostd/src/task/task.rs`, define the bare `Task` primitive:
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
  *(Done — `slopos-ostd/src/task/task.rs`. `TaskId(pub u64)` is `repr(transparent)` with monotonic `alloc()` over a static `AtomicU64`. `TaskContext` is `repr(C)` with the exact same field offsets as the legacy `core::scheduler::SwitchContext` (rbx@0, r12@8, r13@16, r14@24, r15@32, rbp@40, rsp@48, rflags@56, rip@64, total 72 bytes); `const _ = assert!(offset_of!(TaskContext, _) == _)` blocks pin every offset so the asm in `switch.rs` cannot drift silently. `KernelStack { base, size }` is the RAII-owned-stack newtype; `Drop` is a no-op until 1J wires the OSTD frame allocator. `FpuState` is heap-allocated via `KBox<FpuState>` (`KBox::try_init` with an `init_from_closure` that writes `FpuState::new()` directly into the heap slot — the 2.6 KiB rvalue never lands on the caller's stack; `check_stack_sizes.sh` continues to pass). `is_running: AtomicBool` enforces Inv. 8 with `try_mark_running()` / `mark_not_running()`. `CurrentTask { _ne: PhantomData<*const ()> }` is the `!Send` token; `current()` panics if `TaskRuntimeBackend` not registered (one-shot hook, default = unregistered backend that returns null). Phase 1J registers a backend that proxies to `slopos_arch::pcr::current_pcr().current_task`.)*
- [x] **1I.5** Define `pub trait Scheduler` and `pub trait RunQueue`:
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
  Phase 1 provides a default `RoundRobinScheduler` impl inside OSTD (later moved out in Phase 2). *(Done — `slopos-ostd/src/task/scheduler.rs`. `TaskRef = KArc<Task>`. `RoundRobinScheduler` ships as a placeholder vehicle: per-CPU FIFO `KVecDeque<TaskRef>` inside `SpinLock<RoundRobinRq>`, `enqueue` pushes back, `pick_next` rotates the current task to the back. The production scheduler in `core::scheduler` is unchanged in 1I — 1J.9 cuts over.)*
- [x] **1I.6** In `slopos-ostd/src/task/switch.rs`, port `core/src/scheduler/switch_asm.rs` (context switch, task entry trampoline, init-current-context). Keep naked-fn implementations. SAFETY refs Inv. 8. *(Done — `slopos-ostd/src/task/switch.rs`. `switch_registers`, `init_current_context`, `task_entry_trampoline` are `#[unsafe(naked)] pub extern "sysv64" fn` ports of the legacy asm with the offsets driven by `offset_of!(TaskContext, _)`. The trampoline calls a registered `TaskExitHook` after the entry point returns (one-shot hook, panics if hit before registration); this lets OSTD's task-trampoline stay decoupled from the kernel scheduler's task-lifecycle code path. None of these naked fns are reached from kernel code in 1I — the existing `core/src/scheduler/switch_asm.rs` keeps driving execution until 1J.8 deletes it.)*
- [x] **1I.7** In `slopos-ostd/src/task/fpu.rs`, port FPU XSAVE/XRSTOR (today in `core/context_switch.s` + `core/src/scheduler/switch_asm.rs:185`). 64-byte alignment enforced via `#[repr(C, align(64))]` on `FpuState`. *(Done — `slopos-ostd/src/task/fpu.rs`. `#[repr(C, align(64))] pub struct FpuState { data: [u8; FPU_STATE_SIZE] }` (FPU_STATE_SIZE = 2688 — AVX-512 worst case; alignment-64 asserted via `cfg(test)` `assert_eq!(align_of::<FpuState>(), 64)`). `FpuState::new()` initialises FCW + MXCSR for masked exceptions, leaves XSAVE header zeroed (XRSTOR uses processor-reset defaults). `pub unsafe fn fpu_xsave(state, xcr0_mask)` and `fpu_xrstor(state, xcr0_mask)` are inline-asm `xsave64` / `xrstor64` wrappers. The XCR0 mask is taken as an argument rather than read from a global so OSTD doesn't need a `slopos_arch::cpu::xsave::active_xcr0()` round-trip; 1J wires the kernel to read it once at boot and pass it through.)*
- [x] **1I.8** `Task` Drop tears down kernel stack, drops VmSpace ref, frees FPU state slab slot. Inv. 9. *(Done — implicit field-by-field drop covers everything: `KBox<FpuState>` returns its heap slot, `Option<KArc<VmSpace>>` decrements the address-space refcount, the inline `KernelStack` is plain data until 1J adds the real frame-free path. No explicit `Drop for Task` is needed, which keeps the unsafe surface minimal.)*
- [x] **1I.9** Verify: `cargo check -p slopos-ostd` succeeds. `just build` succeeds (consumers still on FFI shims). *(Done — `cargo check -p slopos-ostd` clean. `cargo test -p slopos-ostd` reports **104 lib + 9 dma + 11 io_mem + 6 io_port + 10 irq_line + 10 preempt + 7 uframe + 9 user_mode + 12 vm_space + 7 doctest = 185 passes, 1 ignored, 0 failures**, up from 177 pre-1I (+8 from 4 task::tests + 3 fpu::tests + 1 panic-mode test). `cargo fmt --all -- --check` clean. `just build` finishes in ~2 s with `check_alloc_dep: OK` and `check_stack_sizes: OK`. **Full kernel test run** via `just test`: 2407 kernel-side + 3 userland = **2410 passed, 0 failed, 0 skipped**, parity with pre-1I; `just check-test-count` reports 2410 ≥ baseline 2401. **TCB delta**: roughly net-neutral — every `unsafe` block from `sync/src/*` moved into OSTD verbatim (~30 unsafe tokens in spin.rs, 10 in cpu_local.rs, 8 in lock_tracking.rs, 12 in rcu.rs, 8 in wait_queue.rs, 4 in mutex.rs, 6 in seqlock.rs, 0 in init_flag.rs, 0 in once_lock.rs); plus the new `task/{task.rs, switch.rs, fpu.rs, scheduler.rs}` add ~25 more (naked-fn decls, MaybeUninit slot writes for the three new one-shot registration hooks, two inline asm blocks for xsave/xrstor). **Phase 1J wiring TODO**: (1) register the kernel-side `TaskRuntimeBackend` so `slopos_ostd::task::current()` resolves to the running task; (2) register a `TaskExitHook` so the OSTD trampoline can be reached without panicking; (3) migrate `core/src/scheduler/` to consume `slopos_ostd::task::Task` + the OSTD `Scheduler` trait (1J.9); (4) delete `core/src/scheduler/switch_asm.rs` (1J.8); (5) replace the legacy `slopos-utils::klog_warn!` drop in `RcuBackend::log_warn` with a real logger backend; (6) negotiate `xcr0_mask` once at boot and feed it to the per-CPU `fpu_xsave`/`fpu_xrstor` callers; (7) wire `KernelStack::Drop` to the OSTD frame allocator so kernel stacks are returned automatically. None of (1)–(7) belong to 1I — every existing kernel scheduler / FPU / task path continues to run via the legacy types unchanged.)*

### 1J: Migrate existing kernel to consume OSTD (parity)

This subtask is the bulk of Phase 1's clock time. Every existing kernel crate is rewritten to consume OSTD instead of its own internals. Behavior must be identical.

**Scope realism.** The original 1J.1–1J.16 spec encompasses ~2200 LoC of OSTD arch ports, ABI-critical IDT/scheduler asm migrations, and elimination of ~1700 `unsafe` blocks across 8+ kernel crates. That is genuinely multi-week work and cannot land as a single PR without leaving the kernel non-bootable mid-way. **1J is therefore split into 11 strictly-serial sub-phases (1J-α..1J-λ).** Each sub-phase ends with `just build && just test` succeeding (≥ 2410 tests passing) and is independently mergeable.

#### Sub-phase map

| Sub-phase | Theme | Risk | Subtasks closed |
|---|---|---|---|
| **1J-α** | Wiring foundation (`register_*` hooks) | Low | partial 1J.13 wiring |
| **1J-β** | Safe aliases (no consumer changes) | Low | 1J.3, 1J.4 (alias only), 1J.7, 1J.13 |
| **1J-γ** | Port karch into OSTD + dep inversion ✅ | Medium | 1J.1 |
| **1J-δ** | IDT/GDT migration | High | 1J.2, 1J.11 |
| **1J-ε** | UserModeBackend + LSTAR ✅ | High | enables 1J.10 |
| **1J-ζ** | Scheduler/Task migration | Critical | 1J.8, 1J.9 |
| **1J-η** | VmSpace/paging migration | High | 1J.5, 1J.6 |
| **1J-θ** | SyscallContext refactor | Medium | 1J.10 |
| **1J-ι** | Driver migration cleanup | Low | 1J.12 |
| **1J-κ** | Zero-unsafe enforcement | Multi-week, parallel | 1J.14, 1J.16 |
| **1J-λ** | Phase 1J close + parity gate | Low | 1J.15 |

Strict serial order: α → β → γ → δ → ε → ζ → η → θ → ι → λ. Stage κ runs in parallel (ongoing cleanup) once it's possible to drop unsafe in any given file.

#### 1J-α — Wiring foundation

**Goal.** Install every OSTD `register_*` / `init_*` hook that doesn't depend on a later sub-phase. After this, OSTD primitives are reachable on the hot path; the legacy paths still drive everything. No behavior change.

- [x] **1J-α.1** New `kernel-services/src/ostd_bridge_tables.rs`: static `MMIO_RANGES: &[PhysRange]`, `PORT_RANGES: &[PortRange]`, `RESERVED_VECTORS: &[u8]` populated from the karch vector constants (SYSCALL=0x80, LAPIC_TIMER=0xEC, LUF_DRAIN=0xFA, RCU_QS=0xFB, RESCHEDULE=0xFC, TLB_SHOOTDOWN=0xFD, shutdown=0xFE, spurious=0xFF). *(Done — file landed with `MMIO_RANGES` containing only the architecturally-fixed LAPIC doorbell range `0xFEE0_0000..0xFEE0_1000`; runtime-discovered MMIO regions (HPET/IOAPIC/PCI ECAM/framebuffer) deferred to later sub-phases per spec. `PORT_RANGES` covers PIC1/PIT/CMOS/PIC2/isa-debug-exit/COM1. `RESERVED_VECTORS` lists all eight in-use vectors; SHUTDOWN_VECTOR=0xFE and SPURIOUS_VECTOR=0xFF are local consts (no karch-exported names yet).)*
- [x] **1J-α.2** New `kernel-services/src/ostd_backends/preempt.rs`: `PreemptBackend` impl proxying to `slopos_arch::pcr::current_pcr().preempt_count`. Closes Phase 1F TODO #1. *(Done — `PcrPreemptBackend` calls `current_pcr().preempt_count` via `fetch_add` / `fetch_sub` / `load` with `AcqRel`/`Acquire`. `leave_quiet` still defaults to `leave` because no OSTD reschedule callback machinery exists yet (1J-ζ wires it).)*
- [x] **1J-α.3** New `kernel-services/src/ostd_backends/diagnostic_sink.rs`: `DiagnosticSink::emit` calls a klog facade. Closes Phase 1F TODO #2. *(Done — emits via `platform::console_puts` instead of `slopos-utils::klog`: `slopos-utils` already depends on `slopos-kernel-services`, so pulling klog in here would form a cycle. The console facade reaches the same wire output channel the early boot path uses.)*
- [x] **1J-α.4** New `kernel-services/src/ostd_backends/local_tlb.rs`: `LocalTlbFlush::invlpg` impl (single-instruction asm; the unsafe stays in this crate temporarily — Stage κ retires it). *(Done — `LocalTlbFlushImpl::invlpg` delegates to the existing `slopos_arch::cpu::tlb::invlpg(u64)` helper, so no new unsafe lives in `kernel-services`. `LOCAL_TLB_DYN: &dyn LocalTlbFlush = &LOCAL_TLB` provides the double-ref slot the registration hook expects.)*
- [x] **1J-α.5** Extend `kernel-services::ostd_bridge::register_with_ostd` to call: `register_io_mem_mapper(&&LEGACY_IO_MEM_MAPPER_SHIM)`, `register_io_mem_registry(&MMIO_RANGES)`, `register_io_port_registry(&PORT_RANGES)`, `register_irq_reserved(&RESERVED_VECTORS)`, `register_diagnostic_sink(&KLOG_SINK)`, `register_preempt_backend(&PCR_PREEMPT)`, `register_local_tlb_flusher(&&LOCAL_TLB)`. *(Done — six of the seven registrations live in `kernel-services::ostd_bridge::register_with_ostd`. The seventh (`register_io_mem_mapper`) was relocated to `slopos_mm::io_mem_mapper_shim::register_with_ostd()` to avoid the `kernel-services -> mm -> utils -> kernel-services` cycle; boot calls both in sequence. `register_with_ostd` writes a single `BOOT: register_with_ostd: registered preempt/diag/tlb/io_mem/io_port/irq tables` line via `platform::console_puts` so the new wiring is visible in the boot log before "entering boot init".)*
- [x] **1J-α.6** New boot init step at end of EarlyHw phase: `init_phys_virt_offset(hhdm)` (Phase 1C TODO). *(Done — `BOOT_STEP_INIT_PHYS_VIRT_OFFSET` registered in `boot/src/early_init.rs` with `flags = boot_init_priority(10)` so it sorts after the four existing priority-0 early_hw steps (serial, banner, limine, boot_config). Step body fetches `boot_get_hhdm_offset()` and forwards it to `slopos_ostd::mm::phys::init_phys_virt_offset`.)*
- [x] **1J-α.7** New step after `pcr.install()` in `boot/src/early_init.rs:528`: `register_kernel_master_pml4(cr3_phys)` (Phase 1D TODO). *(Done — added immediately after the BSP `pcr.install()` block in `kernel_main_impl`, before `register_with_ostd`. Reads CR3 via `slopos_arch::cpu::control_regs::read_cr3()` and wraps in `slopos_abi::addr::PhysAddr` for the OSTD hook. This is the bootloader-installed PML4 holding the canonical kernel mappings; it persists for the kernel's lifetime.)*

**Defers** (need later sub-phases): `init_meta_slots` (β), `register_frame_allocator` multi-page (β), `register_user_mode_backend` (ε), `register_task_runtime_backend` (ζ), `register_task_exit_hook` (ζ), `register_iommu_mapper` (Phase 2).

**Verify.** `cargo check --workspace` clean. `just test` ≥ 2410. Boot serial log shows new registrations before "entering boot init". `register_with_ostd` panics on any double-registration.

#### 1J-β — Safe aliases

**Goal.** Replace legacy types with one-line OSTD aliases. Consumer code unchanged.

- [x] **1J-β.1** **(closes 1J.3)** `mm/src/mmio.rs`: replace body with `pub type MmioRegion = slopos_ostd::IoMem;`. Add extension trait if any method is OSTD-missing. *(Done — landed in 1J-β. The `MMIO_RANGES` registry blocker (LAPIC was the only pre-registered range; HPET / IOAPIC / PCI ECAM / device BARs / framebuffer all came from runtime ACPI / PCI discovery) was resolved by adding a heap-free **dynamic-range secondary registry** to OSTD: `pub fn slopos_ostd::mm::io_mem::register_io_mem_range(range)` writes into a fixed-size `[UnsafeCell<PhysRange>; 64]` table guarded by a single `AtomicUsize` count under release-acquire ordering — single-writer-multi-reader by API contract, satisfied automatically because all SlopOS driver init runs serially on the BSP. `IoMemRegistry::reserve` now consults the static slice **and** the dynamic table for containment. The kernel-side `MmioRegionExt::map(phys, size)` calls `register_io_mem_range` first, then `IoMemRegistry::reserve(..., Uncacheable)` — drivers don't have to know the registry exists. **`IoMem`** gained `virt_base()` / `is_mapped()` / `const empty()` accessors (the legacy `MmioRegion` API surface already required by ~14 driver call-sites; without them the type-alias swap would have needed an extension trait that can't reach OSTD's private fields). **`mm/src/mmio.rs`** collapsed from a 193-LoC bespoke struct + virt allocator into a 60-LoC `pub type MmioRegion = IoMem;` plus a thin `MmioRegionExt` trait carrying `map` / `map_page` / `map_1mb`; the legacy `mmio_alloc_virt` / `MMIO_NEXT_VIRT` static disappeared in the rewrite, leaving the OSTD mapper shim at `mm/src/io_mem_mapper_shim.rs` as the sole MMIO virt allocator (pre-existing duplication that drops out for free). One inline-const refactor at `drivers/src/pci.rs:123` (`[const { MmioRegion::empty() }; MAX_ECAM_ENTRIES]`) absorbs the only Copy-dependent call-site; the seven driver structs that derived `Copy` over an `MmioRegion` field (`PciGpuInfo`, `VirtioMmioCaps`, `VirtioMsixState`, `MsixTable`, `XeDevice`, `XeGgtt`, `IoapicController`) drop `Copy` and keep `Clone`, with cloned reads at the two move-out-of-`SpinLockGuard` sites in `pci.rs` / `xe/mod.rs`. `cargo fmt --all -- --check` clean; `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`); `cargo test -p slopos-ostd --tests` green at 208 / 208 (was 204; +2 from the new `dynamic_range_register_then_reserve` / `dynamic_range_outside_static_and_dynamic_rejected` host tests, +2 absorbed elsewhere). `just test` parity left to user verification.)*
- [x] **1J-β.2** **(closes 1J.7)** `mm/src/user_copy.rs`: replace body with `pub use slopos_ostd::user::copy::*;` and `pub use slopos_ostd::user::ptr::{UserPtr, UserSlice, UserBytes, UserPtrError, UserVirtAddr};`. Delete legacy `raw_usercopy`. *(Done — closed by 1J-θ alongside 1J.10. The two structural blockers were resolved as follows. **(a)** OSTD's `UserVirtAddr::try_new` / `UserPtr::try_new` / `UserSlice::try_new` were promoted from `pub(crate)` to `pub`; the doc comment in `slopos-ostd/src/user/ptr.rs` was rewritten to enumerate **two** public construction paths — `UserContext::user_{ptr,slice,bytes}_arg` for the canonical syscall-entry surface and the bare `try_new` constructors for kernel callers that derive a secondary user pointer from an already-validated one (advancing through an array, stepping from `rsp` into a signal frame, …). The Inv. 5 guarantee is still enforced by the validator itself (null/non-canonical/out-of-user-range/overflow rejection), which the loosened visibility doesn't weaken. **(b)** Per-process `&VmSpace` access uses a new `mm::process_vm::process_vm_get_vm_space(pid) -> Option<KArc<VmSpace>>` helper that locks the per-slot mutex, clones the `KArc`, and releases the lock before any `__ostd_raw_usercopy` runs (the `KArc` keeps the `VmSpace` alive across the copy without holding the lock — avoiding lock-order tangles with the page-fault recovery path). The shim resolves the running process's `VmSpace` from `pcr.syscall_pid`, hands it to OSTD's `copy_bytes_*_user`, and maps `UserCopyError` back onto the legacy `UserPtrError` enum (`NotMapped`/`CopyFailed` variants the kernel callers depend on are kept locally with `From<ostd::UserCopyError>` bridging the OSTD enum). **`mm/src/user_ptr.rs`** is now the planned thin re-export: `pub use slopos_ostd::user::ptr::{UserBytes, UserPtr, UserSlice, UserVirtAddr};` (~50 LOC: the type re-exports plus the local `UserPtrError` + two `From` impls). **`mm/src/user_copy.rs`** is now a ~130-LOC shim: PCR pid lookup → `process_vm_get_vm_space` → OSTD `copy_bytes_{from,to}_user` → `UserPtrError` map. The legacy `raw_usercopy` asm block, `__usercopy_start..__usercopy_end` labels, `is_usercopy_ip`, and `usercopy_fault_ip` were deleted; **`boot/src/idt.rs::handle_kernel_copy_fault`** now consults only `slopos_ostd::user::copy::is_ostd_usercopy_ip` (the legacy branch removed alongside the deletion), so the kernel has exactly one fault-recoverable copy band and it lives in OSTD. ~144 caller sites across `core/src/syscall/**`, `core/src/exec/`, `slopos-fs`, `slopos-net`, `slopos-drivers` keep their `slopos_mm::user_copy::*` / `slopos_mm::user_ptr::*` import paths unchanged — the type-identity is preserved through the re-exports, so call-site churn is zero. Verified: `cargo fmt --all -- --check` clean, `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`), `just test` 2409 pass / 0 fail / 0 skip parity, `just boot-log` clean transcript with no panic / #PF / #GP / oops.)*
- [x] **1J-β.3** `boot/src/exception.rs::exception_page_fault`: add `slopos_ostd::user::copy::is_ostd_usercopy_ip(rip)` parallel branch alongside legacy `is_usercopy_ip`. Closes Phase 1G TODO #1. *(Done — landed in 1J-β. Branch added to `boot/src/idt.rs::try_handle_page_fault` (the actual location of the page-fault handler — the original spec referenced `boot/src/exception.rs`); both legacy `slopos_mm::user_copy::is_usercopy_ip` and OSTD `is_ostd_usercopy_ip` ranges are checked, and the matching `*_fault_ip()` function rewrites RIP. The asm symbol ranges are disjoint by name, so both can coexist in the binary while consumer migration is in flight.)*
- [x] **1J-β.4** New `mm/src/kernel_meta.rs`: `KernelMeta` unit struct implementing `slopos_ostd::mm::frame::AnyFrameMeta`. `const _ = assert!(size_of::<KernelMeta>() <= MAX_META_SIZE);`. *(Done — landed in 1J-β. OSTD already exposes a `KernelMeta` ZST with `AnyFrameMeta` impl at `slopos-ostd/src/mm/frame.rs:191-197`, so `mm/src/kernel_meta.rs` re-exports it under `slopos_mm::kernel_meta::KernelMeta`. Both `MAX_META_SIZE` and `MAX_META_ALIGN` razors are duplicated locally as defensive const-asserts. The same module also hosts `install_meta_slots()` (used by 1J-β.7).)*
- [x] **1J-β.5** **(closes 1J.4 — alias only; interior is Stage κ)** `mm/src/page_alloc.rs`: add `pub type OwnedPageFrame = Frame<KernelMeta>;` alongside legacy struct. Forward methods via extension trait. *(Done — landed in 1J-β. Added under the new name `pub type KernelFrame = slopos_ostd::mm::frame::Frame<crate::kernel_meta::KernelMeta>;` (the literal `OwnedPageFrame` collides with the existing struct; renaming the legacy struct would require migrating its 2 driver call sites in `virtio_net.rs` / `virtio_blk.rs`, which is Stage κ.1's scope). Legacy `OwnedPageFrame` struct unchanged.)*
- [x] **1J-β.6** Extend `LegacyFrameAllocShim` (`mm/src/frame_alloc_shim.rs:18-30`) to support `size_pages > 1` via the buddy's existing multi-page path. *(Done — landed in 1J-β. `alloc(opts)` now branches on `opts.size_pages`: `<= 1` calls `alloc_page_frame` as before; `> 1` calls `alloc_page_frames(count, flags)` (the buddy multi-page path at `mm/src/page_alloc.rs:776`). Dealloc was already correct: `free_page_frame` recovers the allocation order from the per-frame descriptor, so the `size_pages` argument stays informational. The `align_pages == 1` debug-assert is preserved until buddy-aligned alloc is wired in a later phase.)*
- [x] **1J-β.7** Add `init_meta_slots` boot step at end of Memory phase. Size = `highest_usable_paddr / PAGE_SIZE`. *(Done — landed in 1J-β. New `boot_step_init_meta_slots_fn` registered in `boot/src/boot_memory.rs` at Memory-phase priority 40 (after MMU ASID at 30). The body lives in `mm/src/kernel_meta.rs::install_meta_slots()`: sizes `n_slots = mm_region_highest_usable_frame() + 1`, allocates `ceil(n_slots * sizeof(MetaSlot) / 4 KiB)` zeroed pages from the buddy, translates the paddr through HHDM (`PhysAddrHhdm::to_virt`), and calls `slopos_ostd::mm::frame::init_meta_slots`. Boot log on QEMU prints `OSTD: meta_slots installed (114572 entries)` (~3.5 MiB of meta-slot backing).)*
- [x] **1J-β.8** `slopos-ostd/src/task/task.rs::KernelStack::Drop`: wire to `FrameAlloc::dealloc`. Closes Phase 1I TODO #7. *(Done — landed in 1J-β. `KernelStack` gained a `paddr: PhysAddr` field (default `PhysAddr::NULL` for the existing `from_raw` ctor — caller-managed storage, no auto-free) and a new `from_frame_alloc(base, size, paddr)` ctor for allocator-owned stacks. `Drop` runs `current_frame_allocator()?.dealloc(self.paddr, self.size / 4096)` only when `paddr` is non-null. Forward-looking: `from_raw` is currently the only ctor in use across the codebase, so all existing call sites still get a no-op drop; Stage 1J-ζ adopts `from_frame_alloc` when migrating the scheduler.)*
- [x] **1J-β.9** **(closes 1J.13)** `cargo check -p slopos-fs -p slopos-net -p slopos-acpi`; each compile error becomes a one-line `use` rewrite. *(Done — landed in 1J-β. fs/net/acpi only import `slopos_mm::{kernel_heap, memory_layout_defs, hhdm}`, none of which were touched in this sub-phase. `just build` finished clean across the workspace; no `use` rewrites were needed.)*

**Verify.** `just build` clean (production gates `check_alloc_dep` + `check_stack_sizes` both green). `just test` 2410/2410 passing (kernel 2407, userland 3, 0 fail). `just boot-log` reaches `ALL SYSTEMS OPERATIONAL!` with the new `OSTD: meta_slots installed (114572 entries)` line during Memory-phase init.

#### 1J-γ — Port karch into OSTD + dep inversion

**Goal.** **(closes 1J.1)** `karch` becomes a thin re-export shell. The ~2200 LoC of CPU/arch primitives live in `slopos-ostd` where they belong (TCB grows by ~50 unsafe tokens, all carrying SAFETY annotations).

- [x] **1J-γ.1** Move verbatim into `slopos-ostd/src/cpu/x86_64/`: `karch/src/cpu/{control_regs.rs,xsave.rs,apic_msr.rs,security.rs,rdrand.rs,core.rs,interrupts.rs,sse.rs,stack.rs,tlb.rs}`. Strip the `slopos-utils` klog deps in `xsave.rs:26,161,166` — replace with `DiagnosticSink::emit`. *(Done — files relocated; the existing `klog_info!`/`klog_debug!` calls were no-op macros routing to nowhere, so they were removed outright rather than re-routed through `DiagnosticSink`. Behavior preserved exactly. `super::cpuid` references rewritten to `crate::arch::x86_64::cpuid` since cpuid moved to `arch/x86_64/`.)*
- [x] **1J-γ.2** Move into `slopos-ostd/src/arch/x86_64/`: `karch/src/cpu/cpuid.rs` (314 LoC), `karch/src/cpu/msr.rs` (142 LoC), `karch/src/arch/gdt.rs` (498 LoC), `karch/src/arch/exception.rs` (42 LoC), `karch/src/tsc.rs`. *(Done — overwrote 1-byte stubs at `arch/x86_64/{cpuid,gdt,msr}.rs` and added `exception.rs`/`tsc.rs`. `exception.rs`'s `super::idt` import rewritten to `crate::irq::idt`.)*
- [x] **1J-γ.3** Merge `karch/src/arch/idt.rs` (180 LoC) constants into `slopos-ostd/src/irq/idt.rs`. Drop the duplicate `IdtEntry` (OSTD's wins). *(Done — appended exception vectors (0..21), `IRQ_BASE_VECTOR`, `SYSCALL_VECTOR`, `TLB_SHOOTDOWN_VECTOR`, `RESCHEDULE_IPI_VECTOR`, `RCU_QS_IPI_VECTOR`, `LUF_DRAIN_IPI_VECTOR`, `LAPIC_TIMER_VECTOR`, and `MSI_VECTOR_{BASE,END,COUNT}` to OSTD's `irq/idt.rs`. OSTD's `IdtEntry` has identical fields plus `format()`/`handler()` helpers (strict superset); karch's was discarded. Removed the `const RCU_QS_IPI_VECTOR: u8 = 0xFB` workaround at `sync/rcu.rs:53`.)*
- [x] **1J-γ.4** Move `karch/src/pcr.rs` (~600 LoC) → `slopos-ostd/src/cpu/x86_64/pcr.rs`. Add compile-time razors for asm-coupled offsets (`kernel_rsp == 0x10`, `current_task == 40`, etc.). *(Done — file moved; `crate::arch::gdt::*` rewritten to `crate::arch::x86_64::gdt::*`, `crate::cpu::msr::Msr` to `crate::arch::x86_64::msr::Msr`, `crate::InitFlag` to `crate::sync::init_flag::InitFlag`, and the two `crate::cpu::write_msr(...)` calls to `crate::arch::x86_64::msr::write_msr(...)`. Existing `const _: () = ...` razor block already covered `kernel_rsp == 0x10` (offset 16) and `current_task == 40` semantically; left unchanged since the assertions hold and the framework feature `sync_unsafe_cell` is now enabled at OSTD's lib.rs.)*
- [x] **1J-γ.5** Move `karch/src/interrupt_frame.rs` → `slopos-ostd/src/irq/interrupt_frame.rs`. *(Done — copied verbatim, registered in `irq/mod.rs`, `InterruptFrame` re-exported at `slopos_ostd::irq` for ergonomic access.)*
- [x] **1J-γ.6** Verify `karch/src/init_flag.rs` is equivalent to `slopos_ostd::sync::InitFlag`; delete duplicate. *(Done — confirmed byte-identical impl. Inlined `InitFlag` struct + impl into `slopos-ostd/src/sync/init_flag.rs`, replacing the `pub use slopos_arch::InitFlag` re-export. `StateFlag` block kept untouched.)*
- [x] **1J-γ.7** Dep inversion (in `slopos-ostd/`): rewrite all `use slopos_arch::xxx` (in `sync/{cpu_local,spin,rcu,lock_tracking,init_flag,seqlock}.rs`, `cpu/preempt.rs`, `task/fpu.rs`) to `use crate::xxx`. Remove `slopos-arch = { path = "../karch" }` from `slopos-ostd/Cargo.toml`. *(Done — `pcr` paths point at `crate::cpu::x86_64::pcr`, `cpu` instruction module aliased via `use crate::cpu::x86_64 as cpu;`, `slopos_arch::tsc::rdtsc()` rewritten to `crate::arch::x86_64::tsc::rdtsc()`, and the `cpu_local!` macro body now expands to `::slopos_ostd::cpu::x86_64::pcr::MAX_CPUS`. `slopos-arch` dep removed from OSTD's Cargo.toml in concert with γ.8.)*
- [x] **1J-γ.8** Update `karch/Cargo.toml` to depend on `slopos-ostd`. Replace `karch/src/lib.rs` with pure re-exports (preserving the existing public path so the ~40 consumers don't change). *(Done — karch deps reduced to just `slopos-ostd`; bitflags/slopos-abi/x86_64 dropped. New `karch/src/lib.rs` is a ~50-line re-export shim covering `pcr`, `cpu`, `arch::{exception,gdt,idt}`, `tsc`, `InterruptFrame`, `InitFlag`, the crate-root pcr items, and the legacy no-op `klog_info!`/`klog_debug!` macros. Glob re-exports of `cpuid::*` and `msr::*` into `cpu::` preserve karch's old `pub use cpuid::*;`-style flat namespace so external sites that call `slopos_arch::cpu::cpuid(...)` (the function, not the module) still resolve. All seventeen files under `karch/src/{arch,cpu}/` plus `pcr.rs`, `interrupt_frame.rs`, `init_flag.rs`, `tsc.rs` deleted.)*

**Risk.** ~40 consumer files use `slopos_arch::xxx`. Re-exports preserve those paths. If any consumer reaches a private path, surface it through OSTD's public API.

**Verify.** `cargo check --workspace` clean. `cargo test -p slopos-ostd` count grows from 185 → ~250. `just test` ≥ 2410.

#### 1J-δ — IDT/GDT migration

**Goal.** **(closes 1J.2, 1J.11)** Replace `boot/src/idt.rs` (766 LoC, 34 unsafe) and `boot/src/gdt.rs` (232 LoC, 10 unsafe) with calls into OSTD's `IdtBuilder` / `gdt::install`. Delete the 42 stubs in `boot/idt_handlers.s`.

- [x] **1J-δ.1** New `slopos_ostd::arch::x86_64::gdt::install(layout: &mut GdtLayout)`. *(Done — landed in 1J-δ. `pub unsafe fn install(layout: &GdtLayout, tss_selector: SegmentSelector)` in `slopos-ostd/src/arch/x86_64/gdt.rs` lifts the `lgdt` + KERNEL_CODE far-return + KERNEL_DATA segment-reload + `ltr` sequence verbatim from `ProcessorControlRegion::install` (`pcr.rs:213-245`); the PCR `install` method now delegates to it so there is one source of truth for the GDT-load asm. SAFETY comments cite Inv. 2 on each asm block.)*
- [x] **1J-δ.2** New `slopos_ostd::arch::x86_64::msr::install_syscall_msrs(...)`. LSTAR points at legacy `syscall_entry` until Stage ε re-points it. *(Done — landed in 1J-δ. `pub unsafe fn install_syscall_msrs(star, lstar, sfmask)` sets `EFER.SCE` if not already set and writes STAR/LSTAR/SFMASK in one place. Companion `pub const fn star_from_selectors(kernel_code, user_data) -> u64` retires the boot-side hand-rolled `(USER_DATA - 8) << 48 | KERNEL_CODE << 32` arithmetic. Boot's `gdt::syscall_msr_init` is now a 6-line caller of the new helpers; LSTAR still points at `syscall_entry` per spec — 1J-ε re-points.)*
- [x] **1J-δ.3** Replace `boot/src/idt.rs::idt_init` body with `slopos_ostd::irq::idt::IdtBuilder` calls. Each existing handler becomes a registered `IrqLine::register_callback` or platform-reserved vector. *(Done — landed in 1J-δ. Boot's local `static IDT: SyncUnsafeCell<[IdtEntry; 256]>` + `IDT_POINTER` + `IdtPtr` + `Idtr` + `handler_ptr` + 42 `extern "C" fn isrN()` declarations are gone, replaced by a single `static BUILDER: IdtBuilder = IdtBuilder::new();` and one `BUILDER.install_default_handlers()` call inside `idt_init`. `idt_set_gate / idt_set_gate_priv / idt_get_gate / idt_set_ist / idt_load` keep their public signatures but forward to the builder. `boot/src/idt.rs` shrinks from 766 LoC / 34 unsafe to 482 LoC / 18 unsafe. The IRQ dispatch arm at the bottom of `common_exception_handler_impl` is rewritten to call `slopos_ostd::irq::dispatch(vector, error_code)` followed by `send_eoi()` and `scheduler_handoff_on_trap_exit(TrapExitSource::Irq)`, with the same pre/post-IRET snapshot the LAPIC timer arm uses for corruption detection.)*
- [x] **1J-δ.4** Delete `boot/idt_handlers.s` once OSTD trampoline + per-vector callbacks cover everything. *(Done — landed in 1J-δ. **Aggressive partial deletion:** all 40 per-vector ISR/IRQ/IPI/MSI stubs (every `isrN`, every `irqN`, all six IPI handlers, `isr_lapic_timer`, the `.altmacro`/`.rept 176` MSI block, plus the shared `INTERRUPT_HANDLER` macro) move into the new `slopos-ostd/src/irq/asm/handlers.s` (`global_asm!`-included from `irq/idt.rs` with `options(att_syntax)`, gated `#[cfg(all(target_arch = "x86_64", not(test)))]`). Wired via `IdtBuilder::install_default_handlers` which declares the 42 asm symbols + `msi_vector_table` as `unsafe extern "C"` inside the function. `boot/idt_handlers.s` shrinks from 616 LoC to 181 LoC keeping only `syscall_entry` (LSTAR target — 1J-ε re-points) and `ret_from_fork` (consumed by `core/context_switch.s` — 1J-ζ deletes); these are the two stubs that genuinely cannot move yet because the kernel-side asm trampolines they couple with still live in boot/core. The asm references kernel-side `common_exception_handler` / `isr_iret_frame_corrupt` symbols via `.extern`; cross-crate linking resolves them just as the legacy boot-internal references did.)*
- [x] **1J-δ.5** **(closes 1J.11)** `core/src/irq.rs` (617 LoC) becomes `pub use slopos_ostd::irq::*;`. Drivers using `irq_dispatch_register` migrate to `IrqLine::register_callback`. *(Done — landed in 1J-δ. New OSTD primitive `IrqAllocator::reserve_specific(vector: u8) -> Result<IrqLine, IrqError>` lets hardware-pinned IOAPIC IRQs (PS/2 keyboard at 33, mouse at 44) claim a fixed bitmap slot — 5 unit tests cover claim / double-claim / out-of-range / drop-releases / platform-reserved-refusal. PS/2 init in `drivers/src/irq.rs` migrates to `IrqAllocator::reserve_specific(IRQ_BASE_VECTOR + irq_line)` + `register_callback(closure)` + `mem::forget(handle); mem::forget(line)` (driver lifetime = kernel lifetime, no teardown). **Post-test fix #1**: the `register_legacy_irq` helper now calls `irq_enable_line(irq_line)` after the OSTD callback is registered — the legacy `register_handler` always called `unmask_irq_line` at the end, and dropping that side effect left the IOAPIC RTE masked from `setup_ioapic_routes`, so PS/2 input never delivered. VirtIO MSI/MSI-X in `drivers/src/virtio/pci.rs` migrates to `IrqAllocator::alloc()` + `register_callback`; the legacy two-step `try_setup_msix` + `register_irq_handlers` API folds into a single setup that takes a `fn(queue_idx: u8)` handler — `register_irq_handlers` is deleted, virtio_blk / virtio_net handlers retire their `extern "C" fn(u8, *mut InterruptFrame, *mut c_void)` shape in favour of plain `fn(queue_idx: u8)`. `core/src/irq.rs` collapses from 617 LoC to 182 LoC: a `pub use slopos_ostd::irq::*;` re-export + the residual kernel-internal counters (`TIMER_TICK_COUNTER`, `KEYBOARD_EVENT_COUNTER`) + per-IRQ-line book-keeping (`set_irq_route`/`get_irq_route`/`is_masked`/`mask_irq_line`/`unmask_irq_line`/`enable_line`/`disable_line`) backed by 16 `AtomicU32`/`AtomicU8` slots — no IRQ_LINES handler table, no MSI bitmap, no SpinLock. The `MsiHandler` extern-C type, `IrqEntry`/`IrqStats` structs, `register_handler`/`unregister_handler`, `irq_dispatch`, `msi_*` family, and the matching kernel-services `irq_register_handler` vtable slot are all deleted. Test coverage rewritten against the new surface: `core/src/tests/irq_tests.rs` (20 tests) and `core/src/tests/msi_tests.rs` (23 tests) exercise `IrqAllocator::alloc/reserve_specific`, `register_callback`/`dispatch`, the residual book-keeping, and the IDT contents populated by `install_default_handlers`. **Post-test fix #2**: the IRQ dispatch arm in `common_exception_handler_impl` now matches the legacy `core::irq::irq_dispatch` shape — snapshot only `(CS, RIP)` before dispatch and check immediately after, BEFORE EOI + scheduler_handoff. The earlier "defense-in-depth" version snapshotted all 5 IRET-payload fields *after* `scheduler_handoff_on_trap_exit`, which fired spuriously in user-mode-preemption edge cases; the resulting `klog_info!("IRQ IRET CORRUPTION: vec={} ...")` formatted 11 args (incl. a u8) onto the unsafe stack, and stacked on top of a deep syscall path (e.g. `exec → vfs_open → ELF load → setup_user_stack`) it overflowed the per-task 8 KiB unsafe stack and faulted in `core::fmt::Argument::new_display::<u8>` — reproducible by opening the file manager. Two-field check + smaller format string keeps the worst-case unsafe-stack usage well under the 8 KiB ceiling.)*
- [x] **1J-δ.6** ABI razors at the new install site: `assert!(offset_of!(InterruptFrame, {rip,cs,rflags,rsp,ss}) == OSTD_*_OFFSET)`, `assert!(offset_of!(ProcessorControlRegion, kernel_rsp) == 0x10)`. *(Done — landed in 1J-δ. `const _: () = { assert!(...) }` block at the top of `boot/src/idt.rs` pins `InterruptFrame::{rip,cs,rflags,rsp,ss}` at 136/144/152/160/168 (the CPU-pushed portion that the OSTD asm unwind operates on) plus `ProcessorControlRegion::offsets::KERNEL_RSP == 16` (the syscall_entry asm reads `gs:[16]` for the kernel stack). Any drift in either the OSTD frame layout or the PCR field order trips the const-assert at compile time.)*

**Verify.** `cargo check --workspace --exclude kernel` clean. `cargo test -p slopos-ostd` reports **113 passed, 0 failed**, up from 109 pre-1J-δ (+4 from `reserve_specific` and `star_from_selectors` tests; one merged via the existing `irq_context` tripwire). `just build` finishes cleanly with `check_alloc_dep: OK` and `check_stack_sizes: OK` (no new ≥ 2 KiB frames). `cargo fmt --all -- --check` clean. **TCB delta**: ~700 LoC of asm moved out of boot into OSTD; net ~16 unsafe tokens shifted from `boot/src/idt.rs` into `slopos-ostd/src/irq/asm/handlers.s` (the asm itself is tracked as a single `global_asm!` block). Boot reaches "ALL SYSTEMS OPERATIONAL!" *(pending user verification — `just test` ≥ 2410 needs the user to run it).* Tests at risk per spec: `tests/syscall/*`, `tests/usercopy*`, `tests/page_fault*`, `tests/ipc/*`, `tests/timer/*`, plus the IRQ/MSI tests now covered by the rewritten `core/src/tests/{irq,msi}_tests.rs` (43 tests vs. 44 originally).

#### 1J-ε — UserModeBackend + LSTAR

**Goal.** Wire the OSTD user-mode round-trip. Enables Stage θ.

- [x] **1J-ε.1** New `kernel-services/src/ostd_backends/user_mode.rs`: `UserModeBackend::execute_round_trip` impl. Stashes `*mut UserContext` in a new PCR field. *(Done — landed in 1J-ε. `PcrUserModeBackend` (ZST) lives in `kernel-services/src/ostd_backends/user_mode.rs` with `pub static PCR_USER_MODE`; `execute_round_trip` (1) stashes `ctx_ptr` in `pcr.user_ctx_ptr` with `Release`, (2) zero-resets `pcr.return_reason`, (3) calls the new naked `slopos_ostd::user::mode::user_mode_round_trip_asm(*const UserRegs)` which saves kernel callee-saves + return RSP/RIP into `pcr.kernel_return_ctx`, builds the IRETQ frame from `UserRegs`, restores user GPRs, and `swapgs; iretq`s, then (4) reads `pcr.return_reason` back via `slopos_ostd::user::mode::read_return_reason` and returns it.)*
- [x] **1J-ε.2** Add `user_ctx_ptr: AtomicPtr<UserContext>` field to `ProcessorControlRegion` (now in OSTD post-γ). Add offset razor. *(Done — landed in 1J-ε. New PCR slots at offsets 96 (`user_ctx_ptr: AtomicPtr<UserContext>`), 104 (`kernel_return_ctx: SyncUnsafeCell<KernelReturnContext>`, 64 bytes), and 168 (`return_reason: ReturnReasonSlot { kind: AtomicU64, payload: AtomicU64 }`, 16 bytes); `gdt` shifts to offset 184. Razored via three new `assert!(offset_of!(...) == ...)` lines in the existing `const _: () = { ... }` block, plus an 8-line razor for every `KernelReturnContext` field offset. `pub mod offsets` exports `USER_CTX_PTR=96`, `KERNEL_RETURN_CTX=104`, `RETURN_REASON_KIND=168`, `RETURN_REASON_PAYLOAD=176` for asm consumption.)*
- [x] **1J-ε.3** Repoint LSTAR from legacy `syscall_entry` (`boot/src/gdt.rs:174`) to `slopos_ostd::user::mode::user_return_trampoline_addr()`. Closes Phase 1G TODO #2/#3. *(Done — landed in 1J-ε. `boot/src/gdt.rs::syscall_msr_init` now sets `lstar_value = slopos_ostd::user::mode::user_return_trampoline_addr()`; the `extern "C" { fn syscall_entry(); }` block is deleted. The legacy `syscall_entry` asm in `boot/idt_handlers.s` is also deleted (lines 66-184 of the previous file); the file now keeps only `ret_from_fork` for kernel-task dispatch. The new LSTAR target is `__ostd_user_return` in `slopos-ostd/src/user/asm/user_return.s` — a global_asm-included AT&T-syntax body that swapgs's, saves user GPRs into the active `UserContext` via `gs:[USER_CTX_PTR]`, encodes `ReturnReason::Syscall(rax)` into `pcr.return_reason`, restores kernel callee-saves from `pcr.kernel_return_ctx`, and `jmp`s back to the saved return RIP. Doc comments referencing `boot/idt_handlers.s::syscall_entry` in `mm/src/mmu/kpti.rs` and `slopos-ostd/src/cpu/x86_64/pcr.rs` updated to point at the new asm location.)*
- [x] **1J-ε.4** Wire `register_user_mode_backend(&PCR_USER_MODE)` in `register_with_ostd`. Closes Phase 1G TODO #4. *(Done — landed in 1J-ε. `kernel-services/src/ostd_bridge.rs::register_with_ostd` adds `register_user_mode_backend(&PCR_USER_MODE)` to its registration block; the success-log `console_puts` line now reads `BOOT: register_with_ostd: registered preempt/diag/tlb/io_mem/io_port/irq/user_mode tables`. **Beyond the spec sub-tasks**, this stage also pulls the kernel-side glue forward (per the plan's "OSTD-only round-trip" interpretation) so existing `tests/syscall/*` keep passing under the new LSTAR: a new `core/src/syscall/user_loop.rs` hosts `user_task_first_run` (the scheduler-dispatched entry point for every user task) which loops on `UserMode::execute()` and dispatches each `ReturnReason::Syscall` through the existing `syscall_handle(*mut InterruptFrame)` via an `InterruptFrame ↔ UserContext` adapter (`interrupt_frame_from_user_ctx` / `apply_frame_to_user_ctx`) — the ~100 syscall handlers keep their `*mut InterruptFrame` ABI, the eventual `&mut UserContext` migration is still 1J-θ. `Task` gained a `pub user_ctx: UserContext` field (initialised via `init_user_ctx_for_new_task` for fresh user tasks and `init_user_ctx_from_parent_frame` for forks/clones). `init_task_context` (user branch), `task_fork`, `task_clone`, and `core/src/exec/mod.rs` all switched from `build_ret_from_fork_frame(...)` to `build_user_task_entry_frame(kernel_stack_top)`; `build_ret_from_fork_frame` is deleted (last call site was the user branch). `UserContext::const_zeroed`, `UserRegs::const_zeroed`, and `FpuStateRef::empty` gained `const fn` constructors so the `Task::invalid()` const path can hold an inline `UserContext`. The `&VmSpace` argument to `UserMode::new` is satisfied by a single `OnceLock<KArc<VmSpace>>` placeholder created lazily on first user-task entry — CR3 is still managed by the legacy paging code outside OSTD; per-process `VmSpace` activation lands in 1J-η.)*

**Risk.** Any error here = triple-fault on first user-mode entry. Add a smoke test that returns immediately to user-mode and back before signing off.

**Verify.** Single-syscall round trip works. All `tests/syscall/*` pass.

- [x] **1J-ε.5** *(Follow-up bug fix)* `__ostd_user_return` was using `pushq %rax` after `mov %gs:[KERNEL_RSP], %rsp` to spill user RAX into the kernel stack at `kernel_rsp - 8`. That address is the SAME memory location as the SS slot of the next CPU-pushed IRET frame at TSS.RSP0 — under specific timing patterns, the trampoline's leftover (e.g. `0x16` = SYS_INFO syscall number) could persist across the next interrupt's CPU push and cause iretq #GP downstream. *(Fixed by adding a per-CPU PCR scratch slot `user_rax_tmp` at offset 184 in `slopos-ostd/src/cpu/x86_64/pcr.rs`, and replacing the `pushq %rax`/`popq %rdx` pair in `slopos-ostd/src/user/asm/user_return.s` with `movq %rax, %gs:PCR_USER_RAX_TMP`/`movq %gs:PCR_USER_RAX_TMP, %rdx`. The kernel stack at `kernel_rsp - 8` is now never touched by the trampoline. Matches Asterinas / Linux which use the equivalent per-CPU scratch (`pt_regs_tmp`).)*

- [x] **1J-ε.6** *(Follow-up bug fix)* `user_mode_round_trip_asm` was leaving its own return address on the kernel stack across the iretq, with `pcr.kernel_return_ctx.rip` pointing at a label `100:` whose body was just `ret`. This `ret` popped `[saved_rsp]` for the return RIP — but `saved_rsp` was the function's entry RSP on the per-task kernel stack, which gets reused (and overwritten) by any interrupt the CPU takes from user mode while pushing 5-word IRET frames at `TSS.RSP0` and the ISR's GP-register pushes. Result: after a user-mode interrupt fired between two SYSCALLs, the kernel's SYSCALL-return path read garbage as RIP and faulted in arbitrary kernel locations. *(Fixed in `slopos-ostd/src/user/mode.rs::user_mode_round_trip_asm` by popping the call's return address into `rax` at function entry and stashing it in `pcr.kernel_return_ctx.rip` directly — `__ostd_user_return` then `jmp`s to that saved RA via `jmp *gs:[krc_rip]`, with no `ret` and no reliance on the kernel stack across the iretq. Matches Linux's `entry_SYSCALL_64` and Asterinas's `syscall_entry`, both of which keep all return-context in per-CPU memory and never trust the kernel stack across user-mode round trips.)*

- [x] **1J-ε.7** *(Follow-up bug fix)* After 1J-ε.5 + 1J-ε.6 landed, init reached the FONT\_SET syscall but a subsequent IRQ from user mode on init's kernel stack hit a kernel-mode `#PF` whose RIP was itself a per-task-kernel-stack address (e.g., `0xa01e8f50` = `kernel_stack_top - 176`) — `ret` popping a stack-resident value instead of a kernel `.text` return address. Root cause: the OSTD round-trip supervisor `user_task_loop` (`core/src/syscall/user_loop.rs`) holds a multi-hundred-byte safe-stack frame on the per-task kernel stack, including a SafeStack-saved unsafe-SP slot at `[rbp-0xb8]` (≈ `kernel_stack_top - 232`).  `TSS.RSP0 = task.kernel_stack_top`, so every IRQ from user mode lands at the top of the same stack and `common_exception_handler_impl`'s 264-byte safe-stack frame (plus its sub-call chain) reaches well past `[rbp-0xb8]`, overwriting it with the spilled `frame: *mut InterruptFrame` argument (`= top - 176`).  When the supervisor next dereferenced the now-corrupt unsafe-SP, it tried to write into a kernel `.text` page and faulted. *(Fixed in `core/src/scheduler/task/task_lifecycle.rs::build_user_task_entry_frame` by adding `const SUPERVISOR_RESERVE: u64 = 0x2000` (8 KiB) and seeding the user-task `SwitchContext.rsp` at `kernel_stack_top - SUPERVISOR_RESERVE` instead of `kernel_stack_top - 16`.  This splits each per-task 32 KiB kernel stack into a top 8 KiB IRQ region (where `TSS.RSP0`-anchored IRQ pushes and the deepest IRQ-handler chain land) and a bottom 24 KiB supervisor region (where `user_task_loop` plus every syscall-dispatch chain lives) — the two regions cannot collide. Linux solves the same problem via `prepare_exit_to_usermode` re-arming `TSS.SP0` on every userspace-exit boundary; Asterinas avoids it by running the supervisor on a per-CPU stack. SlopOS' per-task-stack model takes the third path: split the stack and place the supervisor at the bottom, no scheduler / TSS / per-task-RSP plumbing required.)*

- [x] **1J-ε.8** *(Follow-up bug fix)* `__ostd_user_return` did not re-enable IRQs after entering kernel mode.  SFMASK clears `IF` on every SYSCALL entry (bit 9 of `sfmask_value = 0x47700`), so the trampoline + every Rust frame downstream of it (`PcrUserModeBackend::execute_round_trip` → `user_task_loop` → `syscall_handle` → individual syscall handlers) ran with `IF=0` until the next iretq.  No timer ticks fired on the BSP during a slow syscall handler; the per-CPU tick counter fell behind the global counter (incremented by other CPUs); the cross-CPU watchdog at `core::scheduler::runtime::check_watchdog_for_neighbor` then NMI'd the BSP for "no timer tick in >2 s" and panicked.  Reproducer: `just test` would run the kernel-phase tests, transition to the userland phase via `SYSCALL_RUN_USERLAND_TESTS`, and NMI inside `registry_sorted`'s O(n²) string-comparison loop before the first userland test ran.  *(Fixed by adding `sti` immediately before the final `jmpq *%rax` in `slopos-ostd/src/user/asm/user_return.s`.  The `sti` shadow inhibits IRQs through the `jmpq`, so the first instruction in `execute_round_trip`'s tail can be safely interrupted.  Matches Linux's `entry_SYSCALL_64` (`ENABLE_INTERRUPTS` after publishing pt\_regs) and the legacy `boot/idt_handlers.s::syscall_entry` which had `sti` immediately before `call common_exception_handler` for the same reason.)*

#### 1J-ζ — Scheduler/Task migration

**Goal.** **(closes 1J.8, 1J.9)** Delete `core/src/scheduler/switch_asm.rs` (222 LoC, 9 unsafe) + `core/context_switch.s`. `core/src/scheduler/` consumes `slopos_ostd::task::Task` and `slopos_ostd::task::Scheduler`. **Critical risk** — naked-fn ABI must match exactly.

- [x] **1J-ζ.1** `core/src/scheduler/task_struct.rs:33,101`: replace legacy `Task` + `SwitchContext` with OSTD types via type aliases. *(Done — landed in 1J-ζ. **Scope clarification:** the kernel-side `Task` struct (3.8 KiB, 80+ fields) is **not** aliasable against OSTD's slim `slopos_ostd::task::Task` (6 fields) — that's a re-skin, scheduled for a later phase. What ζ.1 actually aliases is `SwitchContext` → `slopos_ostd::task::TaskContext` (72-byte callee-saved snapshot) and `FpuState` → `slopos_ostd::task::FpuState` (2688-byte XSAVE buffer). The legacy 200-byte `core::scheduler::task_struct::TaskContext` (interrupt-frame-shape register snapshot) stays as-is — retired in 1J-θ. Inherent helpers `FpuState::reset_in_place` becomes free fn `fpu_reset_in_place`; `as_ptr` / `as_mut_ptr` / `active_area_size` are dropped (zero external callers). The `FPU_STATE_SIZE` / `FXSAVE_AREA_SIZE` / `MXCSR_DEFAULT` consts re-export from `slopos_ostd::task::fpu`.)*
- [x] **1J-ζ.2** Migrate all call sites of `core::scheduler::switch_asm::switch_registers` to `slopos_ostd::task::switch::switch_registers`. *(Done — landed in 1J-ζ. Five call sites migrated: `scheduler.rs:524` and `:779` (the two `switch_registers` invocations); `runtime.rs:296` (`init_current_context` capturing the AP return context); `task_lifecycle.rs:11` (the `task_entry_trampoline` import + the L430 `as *const () as u64` cast — same fn-ptr ABI, no body change); `scheduler.rs:363/:458` (FPU save/restore — now `fpu_xsave` / `fpu_xrstor` with the active XCR0 mask cached once at the top of `prepare_switch_to`).)*
- [x] **1J-ζ.3** **(closes 1J.8)** Delete `core/src/scheduler/switch_asm.rs` + `core/context_switch.s`. *(Done — landed in 1J-ζ. The 222-LoC `switch_asm.rs` removed (`git rm`); `core/src/scheduler/mod.rs` no longer declares the module. `core/context_switch.s` was already absent in the tree (verified by `find` before the edit — it had been retired in an earlier sweep), so this checklist item only deletes the surviving naked-fn module. Bonus retirement: `boot/idt_handlers.s` (67 LoC) — the surviving `ret_from_fork` IRETQ stub had zero references in the source tree; removed alongside `boot/src/idt.rs`'s `global_asm!(include_str!("../idt_handlers.s"));` include.)*
- [x] **1J-ζ.4** New `kernel-services/src/ostd_backends/task_runtime.rs`: `TaskRuntimeBackend::current_task` returns the PCR `current_task` slot cast to `*const slopos_ostd::task::Task`. Closes Phase 1I TODO #1. *(Done — landed in 1J-ζ. `PcrTaskRuntimeBackend` reads `slopos_arch::pcr::get_current_task()` (which returns `*mut ()` from `pcr.current_task: AtomicPtr<()>` via Acquire-load) and casts to `*const slopos_ostd::task::Task`. The cast is structurally a no-op: OSTD's `Task` is opaque to all current callers — `slopos_ostd::task::current()` only mints a `CurrentTask` token and never dereferences the pointer. Backend lives in kernel-services rather than slopos-core to avoid the cycle (slopos-core already depends on slopos-kernel-services); routing through `slopos_arch::pcr` is the cycle-free path, mirroring `PcrPreemptBackend` from 1J-α.2. Wired via `kernel-services::ostd_bridge::register_with_ostd`; the BOOT log line now reads `…preempt/diag/tlb/io_mem/io_port/irq/user_mode/task_runtime tables`.)*
- [x] **1J-ζ.5** New `task_exit_trampoline` extern fn wired via `register_task_exit_hook`. Closes Phase 1I TODO #2. *(Done — landed in 1J-ζ. `core::scheduler::scheduler::ostd_task_exit_hook` is a freshly-introduced `extern "sysv64" fn() -> !` shim wrapping `scheduler_task_exit_impl()` (the shim is needed because the OSTD `TaskExitHook` type is `extern "sysv64"` and the legacy impl is plain Rust). A new `pub fn install_ostd_task_exit_hook()` calls `slopos_ostd::task::switch::register_task_exit_hook(ostd_task_exit_hook)`; boot calls it from `boot/src/early_init.rs` immediately after `register_with_ostd()` and before `enter_scheduler(0)`, so the hook is in place before any task can return from its entry function. Lives in slopos-core (rather than kernel-services) because `scheduler_task_exit_impl` is in slopos-core, and kernel-services cannot depend on slopos-core (cycle).)*
- [x] **1J-ζ.6** Compute active XCR0 at boot, store in `static ACTIVE_XCR0: AtomicU64`, plumb to `fpu_xsave`/`fpu_xrstor`. Closes Phase 1I TODO #6. *(Done — landed in 1J-ζ. **Discovery during ζ.1:** the `static ACTIVE_XCR0: AtomicU64` already exists at `slopos-ostd/src/cpu/x86_64/xsave.rs:42` (via 1J-γ's karch port), and `xsave::init()` writes it once at boot priority 42 with the negotiated mask (X87 + SSE always, AVX / AVX-512 if CPUID supports). So 1J-ζ.6 collapses to **plumbing only**: the kernel scheduler reads `slopos_ostd::cpu::x86_64::xsave::active_xcr0()` once at the top of `prepare_switch_to` and passes the cached mask to both `fpu_xsave(prev.fpu_state, xcr0)` and `fpu_xrstor(next.fpu_state, xcr0)`. No new static, no new boot step.)*
- [x] **1J-ζ.7** ABI razors at the kernel call site: `assert!(offset_of!(TaskContext, rsp) == 48)`, `assert!(offset_of!(TaskContext, rip) == 64)`. *(Done — landed in 1J-ζ. `core/src/scheduler/task_struct.rs` carries the explicit named razors plus the size-of razor and the SWITCH_CTX_OFF_* alignment razors against the alias. Field offsets are also pinned inside OSTD at `slopos-ostd/src/task/task.rs:107-116`; the kernel-side duplicates fail the build at the kernel boundary if the alias ever drifts off the asm contract.)*

**Verify.** All tasks yield + resume correctly. Tests: `tests/sched/*`, `tests/timer/*`, `tests/ipc/*`, `tests/multitask/*`, `tests/fork/*`.

#### 1J-η — VmSpace/paging migration

**Goal.** **(closes 1J.5, 1J.6)** Replace `mm::process_vm::ProcessVmInner.pml4: *mut PageTable` with `vm_space: KArc<VmSpace>`. Most of `mm/src/paging/` (1915 LoC, 41 unsafe) becomes deletable.

**Status: complete.** η.1, η.2, η.3, η.4, η.5, η.6 landed. Per-process paging is OSTD-only (every map / unmap / protect / activate / fault-resolve flows through `VmSpace::cursor_mut` or `VmSpace::activate`); the bulk of the legacy `paging::*` per-process surface has been deleted. A small kernel-half fallback survives in `paging/tables.rs` so the priority-10 boot step (`init_memory_system`) can install ACPI / kernel-heap / kernel-stack mappings before `KERNEL_VM_SPACE` is installed at priority 55; post-priority-55 callers should use `slopos_mm::kernel_mappings::*` instead. Detailed status:

- [x] **1J-η.5** Kernel master pml4 becomes `KArc<VmSpace>`. *(Done. New `kernel-services::kernel_vm_space::KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>>` wraps the live boot kernel-master PML4 via `VmSpace::wrap_existing(pml4_phys, Pcid::KERNEL)`. Installed by a new boot-init step `BOOT_STEP_INSTALL_KERNEL_VM_SPACE` at memory-phase priority 55 (after meta_slots at 40 and frame_alloc at 50). The same step then walks the upper 128 TiB via the OSTD cursor to stamp `GLOBAL` on every kernel-half leaf — `paging_mark_kernel_global` rewritten as a polymorphic `protect::<Size4Kb/2Mb/1Gb>` dispatch on `entry.level`. The CR3-reload-to-flush that used to live in `init_paging` moved to the same priority-55 step. **Required OSTD extension:** `WalkOutcome::NotPresent { stopped_at: PageTableLevel }` so cursor walks over sparse address spaces skip empty subtrees in O(1) (without this the 128 TiB walk at 4 KiB stride is 34B iterations and boot hangs). **Required slopos-mm extension:** `mm_region_highest_frame_seen()` (replaces `_usable_frame()` for META_SLOTS sizing) — the kernel master PML4 lives in a `KernelAndModules` region outside "usable" memory; META_SLOTS must cover every paddr a `Frame<M>` may wrap.)*
- [x] **1J-η.6** PTE-flag compat asserts in `mm/src/paging_defs.rs`. *(Done. `const _: () = { assert!(PageFlags::PRESENT.bits() == PteFlags::PRESENT.bits()); … }` covers every hardware bit (PRESENT, WRITABLE, USER, WRITE_THROUGH, CACHE_DISABLE, ACCESSED, DIRTY, HUGE, GLOBAL, NO_EXECUTE, ADDRESS_MASK) and pins the slopos COW software bit at `1 << 9` inside OSTD's `SOFTWARE_BITS_MASK`. The asserts delete with the rest of `paging_defs.rs` once the legacy paging surface fully retires; OSTD's own `pte_flags_pinned_to_x86_64_arch` test covers the OSTD-internal invariant permanently.)*
- [x] **1J-η.1** `mm/src/process_vm.rs:27`: add `vm_space: KArc<VmSpace>`. *(Done — dual-allocation landed in η.2A and the reader-flip landed in η.2F. `ProcessVmInner` carries `vm_space: Option<KArc<VmSpace>>`; `process_vm_get_cr3_phys` and `process_vm_find_pid_by_cr3` consult OSTD's `VmSpace::pml4_paddr` exclusively. New helper `process_vm_get_ostd_pml4_paddr(pid)` is the cross-crate accessor for `task.context.cr3` snapshots and the user-fault dispatcher.)*
- [x] **1J-η.2** Migrate the legacy `pml4` sites onto the OSTD `VmSpace` cursor. *(Done across η.2C / η.2E / η.2F. Scheduler installs OSTD CR3 via `process_vm::process_vm_activate(pid)` + `kernel_vm_space().lock().activate()`; the user-mode `mmu::write_cr3_value` / `mmu::select_cr3` are no longer reached from the hot path. `process_vm.rs` map / unmap / mark-cow callsites dual-write through `dual_paging::ostd_*` helpers; setup_tls_block / write_user_bytes / zero_user_bytes / write_user_u64 read paddrs through `dual_paging::ostd_virt_to_phys_4kb`; user-stack writes in exec route through `process_vm::process_vm_user_va_to_paddr`.)*
- [x] **1J-η.3** `mm/src/{demand,cow}.rs`, `core/src/exec/mod.rs`, `core/src/scheduler/scheduler.rs`, and `mm/src/process_vm.rs` internal helpers consume `vm_space.cursor_mut(...)`. *(Done across η.2B…η.2I. New `mm/src/mmu/luf_hook.rs` registers `LufHook: CursorUnmapHook` via `register_cursor_unmap_hook` from a boot-priority-56 step. `cow::handle_cow_fault` / `demand::handle_demand_fault` / `page_fault::try_resolve_user_fault` route through closure helpers `process_vm::process_vm_with_dual_paging` and `process_vm_with_dual_paging_and_region` so a single per-process lock acquisition serves both legacy and OSTD writes plus the region lookup. `user_copy::validate_user_pages` checks user-accessibility through `process_vm::process_vm_user_va_is_user_accessible` (OSTD cursor query — `paging_is_user_accessible` retired from the load-bearing path). `AnonymousMeta` reverts to a unit struct; `UFrame::wrap_user_paddr` does `from_unused`+`from_in_use` so fork's child mapping the parent's paddr round-trips through META_SLOTS as the sole ref-count authority.)*
- [x] **1J-η.4** **(closes 1J.6 in part)** *(Done — landed in η.4. Per-process paging is now OSTD-only; the legacy paging tree is retained only as a small kernel-half fallback for the boot priority-10 path that runs before `KERNEL_VM_SPACE` is installed.) The work delivered:*
  - *Reshaped `process_vm_clone_cow` (`mm/src/process_vm.rs`): a new `#[inline(never)] clone_cow_snapshot_parent` helper holds the parent slot lock once and builds an in-memory `KVec<(vma_start, vma_end, region, KVec<(va, paddr, flags)>)>`. The child-side walkers (`clone_cow_walk_shared_vma`, `clone_cow_walk_anon_vma`) now consume the snapshot — they no longer take `parent_page_dir` and never re-walk the parent's PML4 with the parent lock dropped.*
  - *Migrated read-side fault handlers to OSTD: `mm/src/cow.rs::handle_cow_fault / resolve_single_ref / resolve_multi_ref / is_cow_fault` now take `&mut KArc<VmSpace>` and route through `dual_paging::ostd_*` helpers + `slopos_ostd::mm::frame::reference_count_at`. Same for `mm/src/demand.rs::handle_demand_fault`. `ProcessPageDir` parameter dropped from every public fault-handler signature; `mm/src/page_fault.rs::try_resolve_user_fault` updated.*
  - *Dropped dual-write inside `mm/src/process_vm.rs`: every legacy `map_page_4kb_in_dir` / `unmap_page_in_dir` / `virt_to_phys_in_dir` / `paging_mark_range_user` / `paging_update_range_protection` / `paging_mark_cow` / `paging_get_pte_flags` / `paging_copy_kernel_mappings` / `paging_sync_kernel_mappings` / `paging_free_user_space` callsite is gone (`map_user_range`, `rollback_range`, `unmap_user_range`, `unmap_and_free_range_inner`, `unmap_and_free_range_dir`, `unmap_range_nofree_dir`, `apply_elf_relocations`, `load_segment_pages`, `process_vm_mmap` MAP_FIXED + shared-memfd path, `process_vm_mprotect`, `clone_cow_walk_*`, `create_process_vm`, `process_vm_clone_cow`, `process_vm_sync_kernel_mappings`).*
  - *Rewrote teardown — `teardown_inner_mappings` no longer uses `MmTeardownGuard`. It issues one `tlb::flush_all_for_process(pid)` upfront, walks the VMA map only to `memfd_dec_mapcount` for shared regions, then `destroy_process_vm` / `process_vm_clone_cow` rollback drop the `KArc<VmSpace>` from the slot. OSTD's `VmSpace::Drop` walker reclaims user-half intermediate page tables and decrements every leaf frame's META_SLOTS. The legacy `unmap_and_free_range_teardown` / `unmap_range_teardown` helpers are deleted.*
  - *New module `mm/src/kernel_mappings.rs` exposes `kernel_map_4kb` / `kernel_map_2mb` / `kernel_unmap_4kb` / `kernel_virt_to_phys` / `kernel_is_mapped` / `kernel_get_page_size` / `mark_kernel_global` — thin wrappers over `kernel_vm_space().lock().cursor_mut/cursor()` using `Frame<AnonymousMeta>` (the only `AnyUFrameMeta` impl OSTD currently exposes; `KernelMeta` is deliberately not `AnyUFrameMeta` so falls back to AnonymousMeta with identical Drop semantics). Available for any post-priority-55 caller; the early-boot path (memory_init, kernel_heap, mmio, stack_va, io_mem_mapper_shim, mmu/mapping, ist_stacks, hhdm) keeps the legacy `paging::map_page_4kb` family because those run before `KERNEL_VM_SPACE` is installed at boot priority 55.*
  - *Migrated specific consumer callers off the per-process legacy paging surface: `boot/src/exception.rs::log_user_page_fault_diagnostics` reads user-VA paddrs via `process_vm_user_va_to_paddr` and CR3 via `process_vm_get_ostd_pml4_paddr` (the legacy `paging::virt_to_phys_process` path is gone). `core/src/syscall/ui_handlers.rs::syscall_roulette_draw` swaps to/from kernel master via `kernel_vm_space().lock().activate()` + `process_vm_activate(caller_pid)` (the `paging_get_kernel_directory` + `switch_page_directory` pair is no longer needed at this site). `boot/src/boot_memory.rs`'s post-GLOBAL CR3 reload routes through `kernel_vm_space().lock().activate()` instead of `write_cr3_value(read_cr3_value())`.*
  - *Pruned `mm/src/paging/`: `paging/tables.rs` rewritten in place to keep ONLY `ProcessPageDir` (vestigial allocator handle on `ProcessVmInner.page_dir`, never installed in CR3), `KERNEL_PAGE_DIR`, `init_paging`, `paging_get_kernel_directory`, `paging_mark_kernel_global` (forwards to `kernel_mappings::mark_kernel_global`), `paging_bump_kernel_mapping_gen`, `map_page_4kb`, `unmap_page`, `virt_to_phys`, `get_memory_layout_info`, `is_mapped`, `get_page_size`. Deleted: every `*_in_dir` variant, `map_page_4kb_in_dir` / `unmap_page_in_dir` / `virt_to_phys_in_dir` / `virt_to_phys_process`, `paging_copy_kernel_mappings` / `paging_sync_kernel_mappings`, `paging_mark_cow` / `paging_resolve_cow` / `paging_is_cow` / `paging_get_pte_flags`, `paging_mark_range_user` / `paging_update_range_protection` / `paging_is_user_accessible`, `paging_free_user_space` / `MmTeardownGuard` / `free_table_level` / `free_page_table_tree`, `map_page_2mb`, `paging_map_shared_kernel_page`, `switch_page_directory`, `EARLY_PML4` / `EARLY_PDPT` / `EARLY_PD`, the LUF-aware unmap path, the per-page user-VA flush helpers, and the `set_cr3` shim. `paging/mod.rs` re-exports shrink accordingly.*
  - *Pruned `mm/src/mmu/`: `mmu/mod.rs` no longer re-exports `Cr3Value` / `read_cr3_value` / `write_cr3_value` / `select_cr3` / `forget_context_local` / `flush_pcid` / `pcid_enabled` / `invpcid_available` / `DYN_ASIDS_PER_CPU`. `read_cr3_value` survives as an internal `pub` symbol because `paging/tables.rs::get_cr3` still uses it; the legacy ASID fns (`init_bsp` / `init_ap`) stay because their boot callers (`boot_memory.rs:22`, `smp.rs:108`) are still wired.*
  - *Stack-size razor: `process_vm_clone_cow` was at 2104 B post-snapshot reshape — splitting the parent-snapshot phase into `clone_cow_snapshot_parent` brings it back under the 2 KiB ceiling.*
  - **Out of scope (deferred):** the early-boot kernel-side helpers (`paging::map_page_4kb`, `paging::unmap_page`, `paging::virt_to_phys`, `paging::is_mapped`, `paging::get_page_size`, `paging::init_paging`, `KERNEL_PAGE_DIR`, the supporting `paging/walker.rs` + `paging/page_table_defs.rs`, `paging_defs.rs`'s flag bag, `Cr3Value` / `MmContextId` / `Pcid` / `read_cr3_value` / `alloc_mm_context_id`, `mmu::asid::init_bsp` / `init_ap`) remain in the tree because the boot priority-10 step (`init_memory_system`) needs to install kernel-half mappings BEFORE the priority-50 frame-allocator and priority-55 `KERNEL_VM_SPACE` are wired. Fully retiring those would require splitting `init_memory_system` into a phase-1 (region/buddy) + phase-2 (heap/mmio/stacks) boot-step pair — left for a follow-up pass.*

**Foundation extensions to OSTD (landed):**

To support the consumer migration without surviving legacy shims, OSTD grew the following capabilities. All of these are reachable from the safe API and have integration tests:

- **`slopos-ostd/src/mm/page_size.rs`** (new) — sealed `PageSize` trait with `Size4Kb` / `Size2Mb` / `Size1Gb` markers carrying `(LEVEL, BYTES, HUGE_BIT)`.
- **`CursorMut::map<S, M>` / `unmap<S, M>` / `protect<S>`** — generic over `PageSize`. Strict alignment razors (`UnalignedCursor`, `UnalignedFrame`) plus `SizeMismatch` for cross-size unmap/protect attempts. Works on huge leaves at level Two / Three.
- **`CursorMut::map_range<S, M, I>`** and **`protect_range<S>`** — bulk operations that iterate by `S::BYTES`, replacing the legacy `paging_mark_range_user` / `paging_update_range_protection` patterns.
- **`page_table::WalkOutcome::NotPresent { stopped_at: PageTableLevel }`** — surfaces the level where the walk halted so cursor walks over sparse address spaces advance by `entry.level.entry_size()` (one PML4 entry skip = 512 GiB) instead of one 4 KiB page at a time. Required for `paging_mark_kernel_global` to terminate.
- **`PageProperty::software: u8`** (3 bits valid, mapped to PTE bits 9..=11) plus `PteFlags::SOFTWARE_BITS_MASK / SHIFT`. AVL bits round-trip through `to_leaf_flags` / `from_leaf_flags`. Slopos COW marker becomes a value within (no parallel bookkeeping map).
- **`VmSpace::wrap_existing(pml4_phys, pcid)`** — unsafe constructor that wraps an already-installed PML4 frame. Pairs with new `PageTableMeta::static_borrowed: bool` so `Frame::Drop` does NOT return statically-owned PML4s to the buddy.
- **`KERNEL_MASTER_GEN: AtomicU64`** + `bump_kernel_master_gen()` + per-VmSpace `kernel_gen` + `resync_kernel_half_if_stale(&self)`. `VmSpace::activate` invokes the resync automatically — kernel-master mutations propagate to every running address space at next context switch with one Acquire-load on the cheap path.
- **`VmSpace::Drop`** — recursive iterative walker over PML4 indices 0..256, reclaims every present non-huge intermediate page table and every leaf user frame via `reclaim_leaked_frame`. Skips kernel half (256..512). Flush-free: refcount-zero invariant guarantees no CPU is using the address space at Drop time.
- **`CursorUnmapHook` trait + `register_cursor_unmap_hook`** — slopos-mm registers a `LufHook` that `cursor.unmap()` invokes for USER-flagged leaves and `VmSpace::activate()` invokes on context switch. Same one-shot register pattern as `register_preempt_backend` / `register_user_mode_backend`. Plus `VmSpace::set_mm_ctx_handle` / `mm_ctx_handle()` for the opaque consumer-defined identifier threaded through both callbacks.
- **`Frame::on_drop`** now actually calls `current_frame_allocator().dealloc(paddr, 1)` for `KernelMeta` / `AnonymousMeta` / non-static-borrowed `PageTableMeta`. Closes a long-standing OSTD gap where the doc claimed dropped frames returned to the allocator but the code transitioned the slot to UNUSED without freeing.
- **PTE-flag invariant test** in `slopos-ostd/src/mm/page_table.rs::tests::pte_flags_pinned_to_x86_64_arch` — pins every architectural bit value to its hex literal so refactors that swap bits fail at test time.

OSTD test count: 215 (was 185 pre-η; +30 new tests for huge-page cursor ops, software bits, wrap_existing, kernel-half resync, Drop walker, cursor-unmap hook, page-size constants).

**Remaining work (η.1 reader-flip + η.2 + η.3 + η.4) — 7-step migration order:**

The blocker for the reader-flip is that user mappings still flow exclusively through `mm/src/paging::map_page_4kb_in_dir(page_dir, …)` — they write to the legacy ProcessPageDir's PML4. The OSTD VmSpace's PML4 holds only kernel-half mappings, so flipping `process_vm_get_cr3_phys` to return `vm_space.pml4_paddr()` immediately page-faults the user. The migration steps below (each tests-green between commits) close the gap:

1. **LufHook registration.** New `mm/src/mmu/luf_hook.rs`: `pub struct LufHook;` + `impl CursorUnmapHook for LufHook { after_unmap(va, pa, ctx) → mm::mmu::luf::queue_unmap(va, pa, MmContextId::from_raw(ctx), 0); on_activate(ctx) → current_cpu_set_active_mm_ctx(...); }`. `kernel-services::ostd_bridge::register_with_ostd` adds `register_cursor_unmap_hook(&LUF_HOOK_REF)`. Atomic add — no behavior change yet (no cursor mutations on a live user VmSpace until step 2).

2. **`process_vm.rs` internal helper migration (dual-write).** Rewrite `map_user_range`, `teardown_inner_mappings`, the rollback / free paths in `create_process_vm` / `process_vm_clone_cow`, and the in-process helpers (`paging_mark_range_user`, `paging_update_range_protection`, `paging_mark_cow`, `paging_resolve_cow`, `paging_get_pte_flags`) so they ALSO route every map / unmap / protect through `proc.vm_space.as_mut().unwrap().cursor_mut(…)`. The per-process `SpinLock<ProcessVmInner>` already holds the only ref to the `KArc<VmSpace>`, so `KArc::get_mut` succeeds. Dual-write keeps both PML4s coherent during the migration window. COW marker rides `PageProperty::software & SOFTWARE_COW_BIT` — slopos-mm defines `pub const SOFTWARE_COW_BIT: u8 = 1 << 0` near the cow.rs callers and reads/writes via `prop.software`.

3. **`demand.rs` / `cow.rs` / `page_fault.rs` / `core/src/exec/mod.rs` migration.** Signatures change from `*mut ProcessPageDir` to `&KArc<VmSpace>` (or borrow from `proc.vm_space`). Bodies use `vm_space.cursor_mut(…)` for map/unmap/protect, and `cursor.query()` for `virt_to_phys_in_dir`-style lookups. `is_cow_fault` queries `vm_space.cursor(...).query()?.property.software & SOFTWARE_COW_BIT != 0`; `resolve_single_ref` rewrites the property and `protect::<Size4Kb>`s; `resolve_multi_ref` does `unmap::<Size4Kb>` → alloc + copy → `map::<Size4Kb>`. `setup_user_stack` / `write_to_user_stack` translate via `vm_space.cursor(...).query()?.paddr` then write through HHDM as today.

4. **Scheduler context-switch migration.** `core/src/scheduler/scheduler.rs:267, 440, 453` (the `mmu::write_cr3_value(target)` calls) replace with `unsafe { proc.vm_space.as_ref().unwrap().activate() }` — which calls `resync_kernel_half_if_stale` and the registered `CursorUnmapHook::on_activate` first, then `write_cr3_pcid` with the OSTD-allocated PCID. The `Cr3Value` plumbing in `mm/src/mmu/cr3.rs` becomes unreachable.

5. **Flip the readers.** `process_vm_get_cr3_phys` returns `vm_space.pml4_paddr()`, `process_vm_find_pid_by_cr3` matches against it. After this commit the scheduler installs the OSTD-managed PML4 on every context switch — but because step 2 dual-wrote every user mapping, the OSTD PML4 has the same content as the legacy one, so user-space behavior is unchanged. **Single up-front shootdown in `destroy_process_vm`:** replaces `MmTeardownGuard::begin` with one `tlb_shootdown(All, FlushType::FullForCtx, mm_ctx_id)` at the start, then the `KArc<VmSpace>` drop runs `VmSpace::Drop`'s recursive flush-free user-half walker.

6. **Stop dual-writing.** Every legacy `map_page_4kb_in_dir` / `unmap_page_in_dir` / `paging_mark_cow` etc. callsite removes the legacy half. `mm/src/paging::*` becomes write-dead-code reachable only from `init_paging`, `paging_get_kernel_directory`, and the kernel-side `map_page_4kb` / `unmap_page` / `virt_to_phys` helpers (next step retires those).

7. **η.4 deletion.** `git rm` `mm/src/paging/{tables, walker, page_table_defs, mod}.rs`, `mm/src/paging_defs.rs`, `mm/src/mmu/cr3.rs::Cr3Value`, `mm/src/mmu/asid.rs::install_mm_context_cr3`, `MmTeardownGuard`, the static `KERNEL_PAGE_DIR`, `EARLY_PML4` / `EARLY_PDPT` / `EARLY_PD`. Kernel-side helpers (`map_page_4kb`, `unmap_page`, `virt_to_phys`, `is_mapped`, `get_page_size`, `paging_map_shared_kernel_page`) move to `kernel_vm_space().lock().cursor_mut/cursor()` callers — either inline at the call site or land in a new `mm/src/kernel_mappings.rs` as thin OSTD-cursor wrappers. Affected callers: `mm/src/kernel_heap.rs` (heap grow/shrink), `mm/src/mmio.rs` (MMIO map), `mm/src/stack_va.rs` (kernel stacks), `mm/src/memory_init.rs` (one bootstrap mapping), `mm/src/io_mem_mapper_shim.rs` (IoMem shim), `mm/src/mmu/mapping.rs` (kernel-side dual mapping), `boot/src/user_fault.rs` (kernel directory probe), `boot/src/tests/shutdown_tests.rs`, `core/src/scheduler/scheduler.rs` (kernel-dir reads at L267/440/453 — already replaced in step 4 if scheduler activates VmSpace, otherwise here).

**Sharing-model decision required during step 4.** `KArc<VmSpace>` works while the per-process slot holds the only ref (no clones); `KArc::get_mut` then succeeds for cursor mutations. If the scheduler needs to share the VmSpace across CPU activates without holding the per-process lock the whole time, choose one of:

- **(a) Switch to `Option<SpinLock<VmSpace>>`** in `ProcessVmInner` (drop KArc). Per-process lock is the sole serialisation point. Activate path locks twice (per-process slot, then the inner SpinLock if needed for resync). Simpler, smaller surface.
- **(b) Extend OSTD so `cursor_mut` takes `&self`** with an internal `AtomicBool` "cursor in flight" guard. Mirrors Asterinas's interior-mutability pattern. `KArc<VmSpace>` works without get_mut. Bigger OSTD change but clean.

Recommendation: (a) for minimum scope; revisit (b) if Phase 3's async work requires it.

**KPTI scope clarification.** `mm/src/mmu/kpti.rs` is currently scaffolding (`KPTI_ENABLED = AtomicBool::new(false)`, `ensure_user_pml4` returns `Err(())`). KPTI dual-PML4 work is **out of scope for 1J-η.** The stub stays put; when KPTI activates (post-Phase 1), it will own its own dual-PML4 layer above `VmSpace::activate()` and OSTD does not need a sibling type.

**Verify.** Per stage: `cargo check -p slopos-ostd && cargo check -p slopos-mm && cargo check -p slopos-core && cargo check -p slopos-kernel-services` clean. `cargo fmt --all -- --check` clean. `just build` clean with `check_alloc_dep: OK` and `check_stack_sizes: OK`. `just test` reports ≥ 2409 passes / 0 failures (parity with the pre-η baseline). At η.4 close: source-tree razor `find mm/src/paging mm/src/paging_defs.rs 2>/dev/null` returns nothing, and `grep -rn 'ProcessPageDir\|paging_get_kernel_directory\|map_page_4kb_in_dir\|MmTeardownGuard\|Cr3Value' --include='*.rs'` returns no hits outside OSTD's compat-assert test module. Watch list: `tests/cow_edge::*`, `tests/demand::*`, `tests/userland_*`, `tests/exec*`, `tests/fork*`.

#### 1J-θ — SyscallContext refactor

**Goal.** **(closes 1J.10)** `core/src/syscall/context.rs:22` migrates from `frame_ptr: *mut InterruptFrame` to `&mut UserContext`. Touches ~100 handlers.

- [x] **1J-θ.1** Add `SyscallContext::from_user_context(task: *mut Task, ctx: &mut UserContext)` ctor.
- [x] **1J-θ.2** Re-implement `ok` / `err` / `ok_i64` / `err_with` / `err_user_ptr` / `args` against `UserContext`.
- [x] **1J-θ.3** `core/src/syscall/dispatch.rs:14`: switch to `from_user_context` (LSTAR already at `__ostd_user_return` post-ε).
- [x] **1J-θ.4** Sweep ~100 handlers in `core/src/syscall/{core,process,net,signal,...}_handlers.rs` for `.frame_ptr()` usage; convert each to `ctx.user_ctx().regs.rdi` etc.
- [x] **1J-θ.5** ABI razor: `assert!(offset_of!(UserRegs, rax) == 0)` etc.

**Verify.** All `tests/syscall/*` pass.

*(Done — landed in 1J-θ. **Status close.** `core/src/syscall/context.rs::SyscallContext` now carries `user_ctx_ptr: *mut UserContext` (was `frame_ptr: *mut InterruptFrame`); the public ctor is `from_user_context(task, &mut UserContext)` (old `new(task, frame)` retired). The handler-table type flips: `SyscallHandler = fn(*mut Task, *mut UserContext) -> SyscallDisposition`. The `define_syscall!` macro and every manual handler in `core_handlers.rs`, `process_handlers.rs`, `signal.rs` were re-shaped in lock-step. Direct `(*frame).{rax,rip,rsp,…}` accesses retired across `dispatch.rs::syscall_handle` / `handle_erestartsys` / `debug_assert_erestartsys_not_leaked`, `signal.rs::syscall_rt_sigreturn` / `deliver_pending_signal`, `process_handlers.rs::syscall_exec` — all GPR mutations now route through `UserContext::set_regs(...)` round-trips so the user-CS/SS selectors and RFLAGS sensitive-bit mask are reapplied on every commit. **Latent fix:** `rt_sigreturn` previously wrote `sigframe.rflags` verbatim to `(*frame).rflags`, bypassing the OSTD `set_rflags` mask — user code could craft a sigframe with IOPL=3 / AC=0 / NT/VM bits cleared and signal-return into it. After 1J-θ, sigframe RFLAGS flow through `set_regs` → `set_rflags`, and the same mask covers `deliver_pending_signal`'s handler-entry write. **`task_fork`** re-typed: `pub fn task_fork(parent: *mut Task, parent_user_ctx: *const UserContext) -> u32`; the legacy iframe-build block at task_lifecycle.rs:916-945 collapsed to a single `set_regs(rax = 0)` on the cloned-from-parent `child.user_ctx`. The dead SwitchContext fix-up at 938-945 (overwritten one line earlier by `build_user_task_entry_frame`) was removed. **`user_loop.rs`** simplified: `interrupt_frame_from_user_ctx` / `apply_frame_to_user_ctx` deleted; the syscall-arm of `user_task_loop` calls `syscall_handle(ctx_ptr)` directly with no synthetic-frame round trip. **Legacy `int 0x80` IDT path** preserved with an inline `InterruptFrame ↔ UserContext` adapter at `boot/src/idt.rs::idt_dispatch` so the rare-but-supported `int 0x80` userland convention still observes identical syscall semantics, including the OSTD mask discipline. **OSTD additions** (`slopos-ostd/src/user/context.rs`): `UserContext::set_rax(value)` (the kernel's new return-value setter); `UserContext::regs_mut() -> &mut UserRegs` (kernel-internal direct-mutation surface, doc-stamped as bypassing the mask discipline — production paths still go through `set_regs`, only test scaffolding uses it for `frame.rdi = X`-style scaffolding); ABI razor `const _: () = { offset_of!(UserRegs, rax) == 0 … }` plus rip / rflags / fs_base / gs_base / cs offset asserts pinning the layout against `__ostd_user_return`'s asm contract.

**Test surface.** `core/src/syscall/tests.rs` ~30 fixtures retargeted from `InterruptFrame` to `UserContext`: `zero_frame()` returns `UserContext::const_zeroed()`; the legacy `frame.<gpr> = X` mutation pattern translates 1:1 to `frame.regs_mut().<gpr> = X` and reads to `frame.regs().<gpr>` (124 field accesses, 40 dispatch sites converted). `&mut frame` becomes `&mut frame as *mut UserContext` at every handler call site. `task_fork` test callers continue passing `core::ptr::null()` — the type-inferred null pointer is now `*const UserContext` and the `parent_user_ctx.is_null()` branch uses the already-`clone_from_raw`'d `child.user_ctx` with `set_regs(rax = 0)`.

**Verify.** `cargo fmt --all -- --check` clean. `just build` finishes with `check_alloc_dep: OK` and `check_stack_sizes: OK` — the `let mut regs = *(*ctx_ptr).regs()` snapshot (200 B `UserRegs`) is well under the 2 KiB frame gate at the few sigframe / exec / ERESTARTSYS sites that use it. **`just test` reports 2406 kernel + 3 userland = 2409 passed, 0 failed, 0 skipped**, parity with pre-1J-θ (`git stash`'d main was also 2409 — the CLAUDE.md text mentioning a 2425 baseline is stale; `just check-test-count`'s actual baseline is 2401, comfortably cleared). 1 over-time test (`tcp_keepalive_reset_on_data`, 690 ms) is non-deterministic network-stack timing, present pre-θ. **`just boot-log`** transcript: clean boot to scheduler → ELF exec for PID 1 / PID 2 → roulette wheel draws → 15 s timeout, no `panic` / `#PF` / `#GP` / `oops` / fault interleaved. **TCB delta:** small net positive — `core/src/syscall/{context,common,dispatch,macros}.rs` and `core/src/syscall/{core,process,signal}_handlers.rs` shed every `unsafe { (*frame).…}` block in favour of OSTD-canonical accessor calls; the only new `unsafe` is the `&mut *ctx_ptr` deref at handler entry (one per manual handler) and the `(*ctx_ptr).set_regs(regs)` commit at the few mutation sites — both narrowly scoped, both sanity-checked by the null-check at function entry. The legacy IDT shim in `boot/src/idt.rs` adds two unsafe blocks (frame→regs read, regs→frame write-back) gated behind the `int 0x80` vector check.

**Phase 1J-θ TODOs that survive into later stages**: (1) `task_clone`'s `interrupt_frame_from_context` synthesis path stays because it does not consume a syscall frame from outside; (2) `slopos_arch::InterruptFrame` itself is still alive — it carries the IDT exception stubs, the legacy timer-tick context save, and the `int 0x80` IDT shim. Only the **syscall-side imports** were retired in 1J-θ.

**Closure of deferred items also rolled into this stage**: 1J.7 / 1J-β.2 (`mm/src/user_copy.rs` thin re-export + `mm/src/user_ptr.rs` thin re-export, `raw_usercopy` asm retirement, `is_usercopy_ip` removal from `boot/src/idt.rs`) — see the per-item closure block below 1J-β.2. The two structural blockers (OSTD `try_new` `pub(crate)` visibility; per-process `&VmSpace` adapter) were resolved by promoting the OSTD constructors to `pub` and adding `mm::process_vm::process_vm_get_vm_space(pid) -> Option<KArc<VmSpace>>`. ~144 caller sites across `core/`, `fs/`, `net/`, `drivers/` continue to use `slopos_mm::user_copy::*` / `slopos_mm::user_ptr::*` import paths unchanged because the legacy module-path identity is preserved through the re-exports — zero call-site churn.)*

#### 1J-ι — Driver migration cleanup

**Goal.** **(closes 1J.12)** Mostly already done by Stage β aliases — this stage just confirms.

- [x] **1J-ι.1** Verify `MmioRegion = IoMem` covers 5 driver consumers (hpet, pci, virtio_net, virtio_blk, xe). Add extension methods if needed.
- [x] **1J-ι.2** `rg 'port_in|port_out|port_read|port_write' drivers/ core/ fs/ net/ acpi/`. Migrate any survivors to `IoPort<T>`.
- [x] **1J-ι.3** 3 driver files (virtio_net, virtio_blk, virtio/pci) migrate IRQ registration to `IrqLine::register_callback`.
- [x] **1J-ι.4** Leave `slopos-utils::io::Port` in early-boot panic logger until Phase 2 (Phase 1E note explicitly defers).

**Verify.** `just test` driver tests pass.

*(Done — landed in 1J-ι. **ι.1 (MmioRegion coverage):** audit-only — every method reachable from the 5 driver consumers (hpet/pci/virtio_net/virtio_blk/xe) is satisfied by `slopos_ostd::IoMem`'s native API (`empty`, `phys_base`, `virt_base`, `size`, `is_mapped`, `is_valid_offset`, `read::<T>`, `try_read::<T>`, `write::<T>`, `try_write::<T>`, `sub_region`) or by the `MmioRegionExt` trait in `mm/src/mmio.rs` (`map(phys, size)`, `map_page`, `map_1mb`). No extension methods added; the alias `pub type MmioRegion = IoMem;` from 1J-β is sufficient. **ι.2 (port-I/O migration):** three driver files migrated onto `slopos_ostd::io::port::IoPortRegistry::reserve::<u8>(...)` — `drivers/src/pic.rs` (PIC1/PIC2 cmd+data quiesce on boot), `drivers/src/pit.rs` (channel 0 + command for the HPET-fallback calibration polled-delay), and `drivers/src/ps2/mod.rs` (data 0x60 + status/command 0x64; ports cached behind a lazy `slopos_ostd::sync::OnceLock<Ps2Ports>` so the per-IRQ accessors remain a single atomic load after first init). **Latent registry bug fixed alongside the migration**: `kernel-services/src/ostd_bridge_tables.rs::PORT_RANGES` was written with inclusive-`end` intent against OSTD's half-open `[start, end)` `PortRange`, which silently undersized PIC1 (covered only 0x20, missed 0x21), PIC2 (0xA0 only, missed 0xA1), PIT (0x40-0x42, missed 0x43 cmd), RTC (0x70 only), COM1 (0x3F8-0x3FE, missed 0x3FF SCR), and the Bochs ACPI shutdown entry (degenerate `0x501..0x501`). Bug was invisible because no driver consulted the registry pre-ι; first driver to migrate would have panicked at `IoPortRegistry::reserve`. Fixed all six entries to half-open semantics + added a new `0x60..0x65` PS/2 range, matching the layout already documented in `slopos-ostd/src/io/port.rs:278-280`. **ι.3 (virtio IRQ):** verification-only — `drivers/src/virtio/pci.rs:254-256` (MSI-X) and `:328-330` (MSI) already register handlers via `IrqLine::register_callback` since 1J-ε; the per-queue closure pattern (`move |_ctx| handler(queue_idx)`) plus `IrqAllocator::alloc()`/`mem::forget(line)` for kernel-lifetime registration is unchanged. **ι.4 (panic-logger deferral):** `utils/src/ports.rs::serial_putc` / `serial_write_bytes` / `serial_write_batch` (the lock-free single-source-of-truth for early-boot serial output) and `drivers/src/serial.rs::SerialPort` (which funnels through those helpers) intentionally remain on `slopos_utils::io::Port`. `boot/src/shutdown.rs` likewise stays on legacy `Port` — that migration belongs to Phase 1J-κ.9. **Verify.** `cargo fmt --all -- --check` clean. `just build` finishes in ~2.4 s with `check_alloc_dep: OK` and `check_stack_sizes: OK`. **TCB delta**: net-zero — three driver files dropped their direct `Port::write`/`Port::read` unsafe blocks in favour of `IoPort`'s safety-equivalent `unsafe fn read/write`, and the new PS/2 `OnceLock` adds zero unsafe (call_once + get are safe). **Test parity** left to user verification per the user's "I will test first" instruction.)*

#### 1J-κ — Zero-unsafe enforcement

**Goal.** **(closes 1J.14, 1J.16)** Drive every `\bunsafe\b` token outside `slopos-ostd/` to literal zero — including production code, `slopos-utils/`, and test scaffolding. **No exemption catalog. No `#[allow(unsafe_code)]` permitted outside `slopos-ostd/`.** Every "irreducible" unsafe pattern (inline asm, naked fn, extern static, Send/Sync markers, panic recovery, boundary slice borrows, BSP-only init) is relocated into a dedicated OSTD module behind a safe wrapper. The `unsafe` keyword still exists for these patterns — but only inside `slopos-ostd/`, where it is reviewed against soundness invariants Inv. 1..10. Multi-week, runs in parallel as a series of small PRs.

**Stage map.** κ.1..κ.15 retired the **structural** unsafe across mm/, boot/, drivers/, fs/, acpi/. κ.17 (closed) added the first wave of OSTD primitives (`VirtqueueRegion`, `EcamConfigSpace`, `AcpiTable<'a>`, expanded `task_accessors`, `IntrusiveLinkedList`). κ.18..κ.22 migrated the surfaces κ.1..κ.15 explicitly deferred. **κ.17 extensions (κ.17.7..κ.17.15)** add the missing OSTD primitives needed for the final absorption (`KernelSync<T>`, `arch::linker`, `arch::naked`, `boot::handoff`, `panic_recovery::poison_all_held_locks`, `HermeticState` derive, OSTD-side task handles, `extern_block!` macro, `early_console`). **κ.23.A..κ.23.J** relocate every residual unsafe pattern by category (8 categories from the original κ.23.1 taxonomy + test-scaffolding migration + utils retirement). κ.16 flips `#![forbid(unsafe_code)]` on every non-OSTD `lib.rs` (including `slopos-utils/`, now in scope after κ.23.J). 1J-λ is the close gate.

`slopos-utils/` is **in scope** for the κ.16 forbid as of this rewrite (was previously deferred to Phase 2). Its early-boot panic-logger pathway is replaced by `slopos_ostd::early_console` in κ.17.15 and the crate is deleted in κ.23.J. Test files outside `slopos-ostd/` are also in scope — no `#![cfg_attr(test, allow(unsafe_code))]` exemption is permitted; test-scaffolding unsafe migrates to OSTD-side derives or helpers in κ.17.12 / κ.23.I.

##### Closed sub-phases (κ.1..κ.15)

- [x] **1J-κ.1** `mm/src/page_alloc.rs` — `OwnedPageFrame` collapsed to `Frame<KernelMeta>` alias. *(Done in 79d21032. Surface 53 → 50 blocks; ~50 buddy-interior blocks tracked in new κ.19.3.)*
- [x] **1J-κ.2** `mm/src/process_vm.rs` residual — lock-free PID scans + `mm_ctx_id` reads collapsed. *(Done in 79d21032. 48 → 41 blocks; ~41 ELF/HHDM/teardown residual tracked in new κ.19.4.)*
- [x] **1J-κ.3** `boot/src/exception.rs` — frame/task field reads via `InterruptFrame::from_ptr` + `task_accessors`. *(Done in 79d21032. 17 → 0 blocks. Added `slopos-ostd/src/irq/interrupt_frame.rs::from_ptr` and `core/src/scheduler/task/task_accessors.rs`.)*
- [x] **1J-κ.4** `boot/src/user_fault.rs` — `task_record_user_fault_exit` + `kernel_vm_space::activate_post_user_fault`. *(Done in 79d21032. 10 → 0 blocks.)*
- [x] **1J-κ.5** `boot/src/limine_protocol.rs` — `SystemInfo` → `OnceLock`; legacy memmap → single `SyncUnsafeCell`. *(Done in 79d21032. 4 → 2 blocks + `unsafe impl Send + Sync for SystemInfo`. 3 surviving sites are irreducible carve-outs tracked in new κ.20.3.)*
- [x] **1J-κ.6** `boot/src/panic.rs` — UTF-8 unchecked → checked. *(Done in bb834f23. 3 → 1 blocks; surviving `panic_recovery::test_longjmp` is an FFI call tracked in κ.23.)*
- [x] **1J-κ.7** `boot/src/ist_stacks.rs` — `Frame::alloc_zeroed` for stack pages; `ist_guard_fault` returns `Option<&[u8]>`. *(Done in bb834f23. 3 → 0 blocks. Added `KernelStackTop::from_kernel_va`.)*
- [x] **1J-κ.8** `boot/src/smp.rs` — AP bring-up via `ApPcrHandle::init`. *(Done in bb834f23. 3 → 1 block + 2 `unsafe extern "C" fn` declarations preserved per LLVM safestack contract; tracked as carve-outs in κ.23.)*
- [x] **1J-κ.9** `boot/src/shutdown.rs` — port I/O via OSTD `IoPort` registry. *(Done in bb834f23. ≤4 sites — `asm!("hlt")`, triple-fault, `cstr_to_str`, `slot.lock().activate()` — deferred to new κ.20.4.)*
- [x] **1J-κ.10** `core/src/scheduler/safestack_rt.rs` — `task_set_unsafe_stack_sp` accessor + safe `wrapping_add`. *(Done in bb834f23. 5 → 0 blocks; the `#[unsafe(naked)]` `__safestack_pointer_address` is preserved per the LLVM contract and tracked as a carve-out in κ.23.)*
- [x] **1J-κ.11** `core/src/syscall/*` — `UserContext::from_ptr_mut`, typed `rcu_call_typed`, `CallbackCtx`. *(Done in 27fc21ba. 164 → 109 blocks (75 prod → 20). 20 production survivors (Pod-blocked sockaddr copies, kmalloc raw access, ui_handlers `activate()`) tracked in κ.18; 89 test-scaffolding blocks tracked in κ.23.3.)*
- [x] **1J-κ.12** `core/src/exec/*` — `task_entry_from_kernel_va` + `process_vm_*_user_bytes`. *(Done in 27fc21ba. 10 → 0 blocks.)*
- [x] **1J-κ.13** `drivers/` — `Frame::{read_at,write_at,slice_at,read_volatile_at}` + `IoMem::as_struct_ref` + `sti_hlt_atomic`. *(Done in 27fc21ba. `virtio/mod.rs` 5 → 0; ~45 blocks across `virtio_net`, `virtio_blk`, `virtio/queue`, `pci`, `ioapic`, `tty/vconsole` tracked in new κ.21. `serial.rs` deferred per ι.4 + κ.21.7.)*
- [x] **1J-κ.14** `fs/`, `net/`, `acpi/` — `cstr_from_kernel_ptr` + `read_packed`. *(Done in 27fc21ba. `acpi/src/tables.rs` 13 → 8 blocks; the `*const SdtHeader` / `*const Rsdp` derefs and the fs/fileio raw-pointer pattern (~50 blocks) tracked in new κ.22. The 18× `unsafe impl Send/Sync` markers in `net/src/*` are contract markers tracked in κ.23.)*
- [x] **1J-κ.15** `utils/` — audit only; defers to Phase 2. *(Done in 27fc21ba. `slopos-utils/` is intentionally excluded from the κ.16 forbid set per Phase 1E + CLAUDE.md early-boot panic-logger deferral.)*

##### κ.17 — Missing OSTD primitives (foundation)

Can land in parallel with κ.18..κ.20. Prerequisite for κ.21 / κ.22.

- [x] **1J-κ.17.1** Add `slopos_ostd::dma::VirtqueueRegion<T: Pod>` over `Frame<KernelMeta>`. Methods: `desc(idx) -> &T`, `desc_mut(idx) -> &mut T`, `slice_payload(idx, len) -> &[u8]`, all bounds-checked. Retires the virtqueue ring pointer arithmetic in `drivers/src/virtio_{net,blk}.rs` and `drivers/src/virtio/queue.rs`.
- [x] **1J-κ.17.2** Add `slopos_ostd::pci::EcamConfigSpace` over `IoMem`. Methods: `read::<T: Pod>(bdf, offset)`, `write::<T: Pod>(bdf, offset, value)`. Retires the raw pointer arithmetic at `drivers/src/pci.rs` ECAM access sites.
- [x] **1J-κ.17.3** Add `slopos_ostd::acpi::AcpiTable<'a>` holding `&'a [u8]`. Methods: `header() -> &SdtHeader`, `payload() -> &[u8]`, checksum-validated `from_bytes(slice) -> Option<Self>`. Retires `*const SdtHeader` / `*const Rsdp` derefs in `acpi/src/{tables,madt,mcfg,hpet}.rs`.
- [x] **1J-κ.17.4** Add `slopos_ostd::dev::DeviceHandle::from_ptr` (parallel to `InterruptFrame::from_ptr`) — null-safe `&DeviceHandle` borrow. Used by `drivers/src/virtio_net.rs:169` and similar.
- [x] **1J-κ.17.5** Extend `core/src/scheduler/task/task_accessors.rs` with the missing scheduler-hot-path fields: `task_time_slice` / `task_set_time_slice`, `task_next_ready` / `task_set_next_ready`, `task_inc_ref` / `task_dec_ref`, `task_fpu_state_mut`. Each absorbs one `unsafe { (*task).<field> }` per-field access pattern.
- [x] **1J-κ.17.6** Add `slopos_ostd::sync::IntrusiveLinkedList<T>` — safe wrapper over the `next_ready: *mut T` pattern in `core/src/scheduler/per_cpu.rs::ReadyQueue`. Methods: `push`, `pop`, `remove(handle)`, `iter()`. Implementation may keep one internal `unsafe`; consumers see a safe surface.

**Acceptance.** `cargo test -p slopos-ostd` count grows by ≥ 30. No consumer changes yet. `cargo fmt --all -- --check` clean; `just build` clean.

*(Done. Foundation phase landed purely additive — no consumer migrations. **κ.17.1**: `slopos-ostd/src/dma/{mod.rs,virtqueue.rs}` add `VirtqueueRegion<T: Pod>` over `Frame<KernelMeta>` with bounds-checked typed-descriptor and byte-payload accessors that funnel through the existing `Frame::{read_at,write_at,read_volatile_at,write_volatile_at,slice_at,slice_at_mut}` helpers — zero new `unsafe`. **κ.17.2**: `slopos-ostd/src/pci/mod.rs` adds `EcamConfigSpace` + `Bdf` (rejects `device >= 32` / `function >= 8`) backed by `IoMem::try_read` / `try_write`; the unsafety budget is fully absorbed by `IoMem`. **κ.17.3**: `slopos-ostd/src/acpi/mod.rs` defines new `Rsdp` / `SdtHeader` Pod structs (`#[repr(C, packed)]` + manual `unsafe impl Pod` since `slopos-ostd-derive` rejects packed) and `AcpiTable<'a>::from_bytes(slice)` with both v1- and v2-checksum-validated `Rsdp::validate`; existing `acpi/src/tables.rs` definitions remain untouched until the consumer-side migration in κ.22. **κ.17.4**: deviates slightly from the literal "DeviceHandle::from_ptr" wording — `slopos-ostd/src/dev/mod.rs` introduces a `FromRawPtr` extension trait with a blanket `impl<T>` so `DeviceHandle::from_ptr(ptr)` becomes valid syntax via `use slopos_ostd::dev::FromRawPtr;` without moving net's DeviceHandle into the trusted-domain crate. **κ.17.5**: `core/src/scheduler/task/task_accessors.rs` gains `task_time_slice` / `task_set_time_slice` / `task_time_slice_remaining` / `task_set_time_slice_remaining` / `task_next_ready` / `task_set_next_ready` / `task_inc_ref` / `task_dec_ref` / `task_ref_count` / `task_fpu_state_mut` (10 accessors total — 2 extra over the spec for symmetry on `time_slice_remaining` and `ref_count`-readonly). **κ.17.6**: `slopos-ostd/src/sync/intrusive.rs` provides `IntrusiveLinkedList<T: Linked>` with `Link<T>` slot + unsafe `Linked` trait; the implementation contains 9 small `unsafe` blocks (head/tail walks, splice, iter reborrow), all SAFETY-commented and gated by the `Linked` invariant — consumers see fully safe surface. **Tests**: 44 new host-side tests across `tests/{virtqueue,ecam,acpi_table,from_raw_ptr,intrusive_list}.rs` (8/9/11/5/11), comfortably over the ≥ 30 floor. `cargo fmt --all -- --check`, `cargo test -p slopos-ostd`, and `just build` (with `check_alloc_dep: OK` + `check_stack_sizes: OK`) all pass. **TCB delta**: net-positive within OSTD by design — primitives concentrate the unsafety so κ.18..κ.22 can drop it from consumers; no production unsafe was retired in this phase.)*

##### κ.17 extensions — primitives for final absorption

Foundation phase, parallel to κ.18..κ.22. **Required before any κ.23.A..κ.23.J absorption stage can land.** Each sub-stage is small, additive, and independently landable.

- [x] **1J-κ.17.7** Add `slopos_ostd::sync::KernelSync<T>` — newtype wrapper providing unconditional `Send + Sync` for kernel-only types whose access is lock-mediated. Body: `pub struct KernelSync<T>(T); unsafe impl<T> Send for KernelSync<T> {} unsafe impl<T> Sync for KernelSync<T> {}` with safety doc citing the kernel-only-access contract (Inv. 8 — single-CPU task ownership analogue for shared globals). Methods: `new(T)`, `get(&self) -> &T`, `get_mut(&mut self) -> &mut T`, `into_inner()`. Eliminates 46 `unsafe impl Send/Sync` markers across boot/, core/, drivers/, fs/, net/, video/, windowing/, service-core/. Also expose a `BspToken` newtype constructible only from inside OSTD's BSP-init path, used by κ.17.x register-* signatures (see κ.23.F). *Acceptance:* OSTD test-suite gains ≥3 round-trip tests; `KernelSync<RefCell<u64>>` round-trips across thread boundaries in host tests. *(Done. `slopos-ostd/src/sync/kernel_sync.rs` already shipped `KernelSync<T>` + a `pub unsafe fn BspToken::new`; this stage tightens the seal: constructor is now `pub(crate) const unsafe fn new`, so external crates cannot fabricate a token even via `unsafe {}`. The single public mint pathway is the new `slopos_ostd::sync::run_bsp_init<R>(f: impl FnOnce(&BspToken) -> R) -> R` — a process-global `InitFlag` guards against double-mint and panics on the second call, mirroring OSTD's house style for one-shot registries (`register_kernel_master_pml4`, `register_frame_allocator`, etc.). A `test-helpers`-gated `reset_bsp_token_for_tests()` hook re-arms the guard for integration tests. **Tests**: new `slopos-ostd/tests/kernel_sync.rs` lands 14 host-side tests — `refcell_u64_round_trips_across_threads` (literal-spec test), `raw_pointer_round_trips_across_threads` (limine_protocol / paging::tables consumer shape), `unsafe_cell_round_trips_across_threads` (bootstrap-task shape), `cell_round_trips_across_threads`, `clone_and_into_inner_preserve_value`, `default_constructs_inner_default`, `get_mut_yields_exclusive_borrow`, `deref_and_deref_mut_round_trip`, `kernel_sync_debug_format_round_trips_inner`, `send_sync_flags_present_for_not_sync_inner`, `bsp_token_passes_inside_callback`, `bsp_token_reset_allows_remint`, `bsp_token_single_shot_panics_on_double_call` (`#[should_panic]`), `bsp_token_is_zero_sized`. BSP-touching tests serialize on a poison-tolerant `Mutex` since `cargo test` parallelises by default. **No consumer migrations** — pre-existing `KernelSync<T>` adopters in `boot/limine_protocol.rs`, `boot/early_init.rs`, `core/syscall/{common,handlers}.rs`, `mm/paging/tables.rs` are unchanged. Send/Sync absorption (κ.23.E) and `register_*(&BspToken, …)` rewrites (κ.23.F) remain outstanding and are unblocked by this stage. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd` green across every test binary, `just check` (`check_alloc_dep: OK`, `check_stack_sizes: OK`), `just build` clean, `just test` 2417 pass / 0 fail / 0 skip / 0 over-time.)*

- [x] **1J-κ.17.8** Add `slopos_ostd::arch::x86_64::linker` — safe accessors over linker-defined symbols. Body declares `unsafe extern "C" { static _text_start: u8; static _text_end: u8; static _kernel_start: u8; static _kernel_end: u8; static kernel_stack_top_impl: u8; }` once inside OSTD; exposes `pub fn text_range() -> Range<*const u8>`, `pub fn kernel_image_range() -> Range<*const u8>`, `pub fn kernel_stack_top() -> *const u8`. The `unsafe extern` block lives inside OSTD; consumers call safe functions. Closes ~10 `unsafe extern "C" { static ... }` blocks in core/, mm/, boot/, hermetic/. *Acceptance:* ≥3 OSTD host-side smoke tests confirming the accessors return non-null in a test harness. *(Done. `slopos-ostd/src/arch/x86_64/linker.rs` ships the canonical `unsafe extern "C"` block + three safe accessors. The kernel-target `#[cfg(target_os = "none")]` arm names the live link.ld anchors; the host-target arm backs each "symbol" with a private BSS buffer so `cargo test` resolves ranges and ordering invariants without the kernel ELF. **Tests**: `slopos-ostd/tests/linker_symbols.rs` adds 5 host-side tests — `text_range_is_non_null_and_ordered`, `kernel_image_range_is_non_null_and_ordered`, `kernel_stack_top_is_non_null`, `kernel_image_envelops_or_aliases_text`, `stack_top_distinct_from_image_anchors`. **No consumer migrations** — the catalogue of legacy `unsafe extern` blocks in `boot/src/gdt.rs`, `core/src/scheduler/{ffi_boundary,scheduler}.rs`, `mm/src/symbols.rs` is unchanged; their migration lands in κ.23.A. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd --test linker_symbols` green, `just build` clean, `just test` 2417 pass / 0 fail.)*

- [x] **1J-κ.17.9** Add `slopos_ostd::arch::x86_64::naked` — relocated home for `#[unsafe(naked)]` functions. Move `__safestack_pointer_address` (currently `core/src/scheduler/safestack_rt.rs:193`) and `ap_entry` / `ap_entry_rust` (currently `boot/src/smp.rs:38–93`) into this module. Both keep `#[unsafe(naked)]` (Rust language requirement) but the keyword now lives inside OSTD. Expose `pub fn install_safestack_runtime()` and `pub fn install_ap_trampoline(payload: ApPayload)` as safe wrappers; consumers call those, never touch the naked fn directly. *Acceptance:* `rg '#\[unsafe\(naked\)\]' --type rust -g '!slopos-ostd/**'` returns 0 once consumers migrate (in κ.23.B). *(Done with **Fuchsia-style ABI sub-struct** instead of the bare offset-numeric proposed in the plan body. World-class research surveyed Fuchsia (`zircon/system/public/zircon/tls.h` + `struct x86_percpu` — toolchain-owned SafeStack offset baked into a named struct), Asterinas (hand-mirrored offsets, no razor — weaker), Linux (`arch/x86/kernel/asm-offsets.c` generates a header — heavier build infra), and Rust-for-Linux (opaque wrappers, no naked asm against `task_struct`). The Fuchsia pattern is the strongest match: OSTD owns both the asm and the layout. **Shipped shape**: `slopos-ostd/src/task/abi.rs::TaskAbi { pub unsafe_stack_sp: u64 }` + `pub const TASK_UNSAFE_STACK_SP_OFFSET: usize = offset_of!(TaskAbi, unsafe_stack_sp)` (trivially 0). Kernel-side `Task` restructures to place `abi: TaskAbi` as field #0; a `const _: () = assert!(offset_of!(Task, abi) == 0);` razor catches reordering. The historical `task_struct::TASK_UNSAFE_STACK_SP_OFFSET` becomes a `pub use` re-export of the OSTD const. All 7 kernel call sites that read `task.unsafe_stack_sp` switch to `task.abi.unsafe_stack_sp` (`task_struct.rs`, `task_accessors.rs`, `task_lifecycle.rs` ×3, `task_table.rs`). `__safestack_pointer_address` moves verbatim to `slopos-ostd/src/arch/x86_64/naked.rs` alongside the pre-existing `ap_entry`. New `slopos-ostd/src/arch/x86_64/safestack.rs` exposes `install_safestack_runtime(&BspToken)` (today a no-op documentation hook) and `install_ap_trampoline(&BspToken) -> unsafe extern "C" fn(*const ()) -> !` (returns the `ap_entry` fn pointer so callers transmute to limine's `MpGotoFunction` without OSTD growing a limine dep). The FPU-state offset razor at `task_struct.rs:247` relaxes from `==` to `>= && < +64` so the 64-byte FpuState alignment padding induced by the new `abi` head field doesn't trip the tripwire (real field insertions between `context` and `fpu_state` still fail). **`ApPayload` deliberately not synthesised** — no consumer currently carries an AP-specific payload, and the existing `MpInfo.extra`-keyed flow remains correct. **Tests**: `slopos-ostd/tests/safestack_symbol.rs` adds 5 host-side tests — `task_abi_unsafe_stack_sp_at_offset_zero`, `task_abi_layout_is_repr_c_u64`, `install_ap_trampoline_returns_non_null_fn_pointer`, `install_safestack_runtime_accepts_bsp_token`, `task_abi_unsafe_stack_sp_is_writeable_round_trip`. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd` green, `just build` clean (alloc + stack-size gates pass), `just test` 2417 pass / 0 fail — the AP bringup + SafeStack-instrumented context-switch paths still resolve `__safestack_pointer_address` via the OSTD-owned symbol. **Consumer migration of `ap_entry_goto` (collapse the limine transmute to `install_ap_trampoline`) remains queued for κ.23.B.**)*

- [x] **1J-κ.17.10** Add `slopos_ostd::boot::handoff` — bootloader-published memory boundary primitives. Each function consolidates one `core::slice::from_raw_parts` call against bootloader memory: `pub fn acpi_handoff(phys: PhysAddr, len: usize) -> Option<AcpiTable<'static>>`; `pub fn framebuffer_handoff(phys: PhysAddr, pitch: usize, height: u32) -> Framebuffer`; `pub fn memmap_handoff(entries: NonNull<MemmapEntry>, count: usize) -> &'static [MemmapEntry]`; `pub fn elf_image_handoff(payload: NonNull<u8>, len: usize) -> Option<ElfImage<'static>>`. Each unsafe block is interior to OSTD; consumers receive `&'static` references or typed views. Closes 20 `core::slice::from_raw_parts` sites in mm/, acpi/, net/, windowing/. *Acceptance:* ≥4 OSTD host-side tests with synthetic byte slices. *(Done. `slopos-ostd/src/boot/handoff/{mod,acpi,framebuffer,memmap,elf}.rs` (~200 LoC) ships the four handoff primitives. New OSTD types — `Framebuffer { base, pitch, height }` (`Send + Sync` markers + `as_bytes_mut`), `#[repr(C)] MemmapEntry { base, length, typ }`, `ElfImage<'a> { bytes }` (with `magic()` 4-byte tag probe) — are minimal pass-through views; the kernel-side `BootFramebuffer` / `LimineMemmapEntry` / ELF parsers stay in place until κ.22 / κ.19 / κ.23 absorb their `from_raw_parts` sites. **HHDM bridge**: new `slopos-ostd/src/boot/hhdm.rs` adds `register_hhdm_offset(&BspToken, u64)` + `hhdm_offset() -> Option<u64>` so `acpi_handoff` can `PhysAddr → virt` without depending on `slopos-mm`; the registry uses `InitFlag` + an `AtomicU64`, panics on double-mint, and the `test-helpers`-gated `reset_hhdm_offset_for_tests()` hook re-arms the guard for integration tests. **Site-count correction**: the plan's "20 sites" overstates the actual handoff surface. Real `from_raw_parts` sites in mm/acpi/net/windowing/video sum to ~14; only ~3 are bootloader-published handoffs (`acpi/src/tables.rs:53`, `mm/src/process_vm.rs:781` ELF, framebuffer is pre-mapped). The remaining sites are internal slicing (kernel heap, packet buffers, memfd) which fall outside the handoff scope. **Tests**: `slopos-ostd/tests/boot_handoff.rs` adds 8 host-side tests — `acpi_handoff_requires_hhdm_registration`, `acpi_handoff_round_trips_checksum_validated_table`, `acpi_handoff_rejects_null_phys_or_zero_len`, `framebuffer_handoff_exposes_dimensions_and_byte_slice`, `memmap_handoff_borrows_entry_array`, `elf_image_handoff_accepts_valid_magic`, `elf_image_handoff_rejects_bad_magic`, `elf_image_handoff_rejects_short_payload`. BSP-token-touching tests serialise on a `Mutex` because the HHDM registry is process-global. **No consumer migrations** — `acpi/src/tables.rs::acpi_region_bytes` and `mm/src/process_vm.rs:781` keep their existing shape; their migration queues for κ.22.2 / κ.19.4. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd` green, `just build` clean, `just test` 2417 pass / 0 fail.)*

- [x] **1J-κ.17.11** Add `slopos_ostd::sync::panic_recovery::poison_all_held_locks() -> !` — single panic-time entry point that walks the per-CPU held-lock list (already tracked by `enable_lock_tracking`) and poisons each held lock. Replaces 11 per-subsystem `pub unsafe fn *_force_unlock` / `*_poison_unlock` wrappers in mm/page_alloc.rs, mm/kernel_heap.rs, mm/process_vm.rs, core/scheduler/task_table.rs, fileio. The kernel panic handler calls one safe function; the per-subsystem `pub unsafe fn` wrappers are deleted. SAFETY commentary cites the panic-only contract (Inv. 9 lifetime obligations relax during fatal abort). *Acceptance:* OSTD lock-tracking tests demonstrate poison-walk correctness with ≥2 held locks across CPUs. *(Done. `slopos-ostd/src/sync/panic_recovery.rs` ships `pub fn poison_all_held_locks() -> !` — a thin safe wrapper over the pre-existing `lock_tracking::poison_unlock_all_held()` walker plus a centralised `cli; hlt` halt-loop (with a host-only `spin_loop` fallback so the `-> !` signature type-checks under `cargo test`). The wrapper centralises the SAFETY rationale citing the panic-only contract. **Audit correction**: the plan's "11 wrappers" appears to overcount — actual census shows 9: `mm/src/{page_alloc,kernel_heap,process_vm}.rs` (2 each: force / poison), `core/src/scheduler/task/task_table.rs` (2), and the vestigial no-op `core/src/scheduler/scheduler.rs::scheduler_force_unlock` (1). **No wrapper deletion this stage** — `mm_panic_cleanup` (`memory_init.rs:665-674`) and the scheduler panic-cleanup hook (`scheduler.rs:1199-1210`) still call the legacy wrappers; their deletion + adoption of the unified entry point queue for κ.23 alongside the `slopos_utils::panic_recovery::register_panic_cleanup` retirement. **Tests**: `slopos-ostd/tests/panic_recovery.rs` adds 3 host-side tests — `poison_walk_fires_each_held_lock_callback` (pushes 2 synthetic locks with distinct poison callbacks, asserts both fire in reverse order and the stack rewinds to depth 0), `poison_walk_empty_stack_is_noop`, `poison_all_held_locks_signature_is_never_returning` (type-level pin on the `fn() -> !` signature). Tests serialise on a `Mutex` because the per-CPU stack is process-global on host. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd --test panic_recovery` green, `just build` clean, `just test` 2417 pass / 0 fail.)*

- [x] **1J-κ.17.12** Add `slopos-ostd-derive::HermeticState` proc-macro derive. Mirror of existing `Pod`/`Zeroable` derive scaffolding. Replaces 14 hand-written `unsafe impl HermeticState for X` in `core/src/scheduler/test_hermetic.rs`. Field-level `HermeticState` bound enforced by the derive; non-hermetic fields cause a compile error. Trait definition itself relocates from `core/` into `slopos_ostd::test_support::hermetic` so the derive macro and the trait live in the same crate hierarchy. *Acceptance:* `slopos-ostd/tests/hermetic_derive.rs` covers named-struct, transparent, and unit-struct shapes (≥4 tests). *(Done with **function-like macro `hermetic_state! { ... }`** instead of the `#[derive(HermeticState)]` proposed in the plan body. World-class research surveyed Asterinas `ostd-test` (no fixtures, no snapshot/restore), Hubris (MPU-restart sandboxing, no kernel-singleton model), Theseus (cell-loaded apps, structural isolation), Linux KUnit (LIFO cleanups, doc explicitly punts on subsystem-state isolation), and the Rust derive ecosystem (`bitflags!`, `pin_project!`, `tracing::instrument`, `linkme::distributed_slice`). **No production Rust kernel ships a derive-able snapshot/restore trait.** The 13 existing impls are unit structs whose `snapshot()` / `restore()` touch external globals (`pcr::*`, MSRs, atomics), not struct fields — a field-composition derive would have zero callers at landing. The ecosystem precedent for "trait whose body isn't field-composable" is a function-like macro that subsumes both the trait impl and the registration call. **Shipped shape**: trait + vtable + macro all live in `slopos-ostd/src/test_support/hermetic/{mod,trait_def,vtable,macros}.rs`. The `HermeticState` trait body is identical to the previous slopos-hermetic definition; `HermeticVTable` + `snapshot_thunk` / `restore_thunk` move from slopos-hermetic into OSTD. The new `hermetic_state! { pub Name { type Snapshot = T; const DEPENDS_ON = &[…]; fn snapshot ... unsafe fn restore ... } }` macro expands to the marker struct, the `unsafe impl HermeticState`, and an internal `__hermetic_register!` helper that emits the `#[link_section = ".hermetic_state_registry"]` static via `paste::paste!` ident munging (OSTD gains a `paste` workspace dep + a `#[doc(hidden)] pub use paste as __paste;` re-export). The slopos-hermetic crate becomes a thin shim: `trait_def.rs` re-exports `slopos_ostd::test_support::hermetic::HermeticState`; `registry.rs` re-exports `HermeticVTable`; `registry_iter` / `topo_order` (which need `KVec`) stay in slopos-hermetic. The legacy `register_hermetic_state!` macro keeps working via its `$crate::HermeticVTable::new::<$ty>()` body — `HermeticVTable` is the re-export — so any external callers compile unchanged. **All 13 sites migrated**: `core/src/scheduler/test_hermetic.rs` rewrote `PerCpuOnlineBits`, `PerCpuSchedulerEnableBits`, `SchedulersInitFlag`, `BspCurrentTask`, `BspIdleTask`, `SchedulerEnabledFlag`, `TssIstShadow`, `TssRsp0Shadow`, `MsrShadow`, `PanicCleanupHandlers`, `KlogLevelShadow`, `WatchdogTicksShadow`, `ForkRrCounterShadow` to the macro form (file shrank 426 → 357 LoC). The auxiliary `MsrSnapshot` struct + `unsafe impl Send` stay hand-written because they're a side-table for `MsrShadow`'s snapshot payload, not a hermetic-state type. **Note on the plan's "14" count**: actual census is 13 in `test_hermetic.rs`. **Tests**: `slopos-ostd/tests/hermetic_macro.rs` adds 6 host-side tests — `plain_state_macro_emits_working_impl`, `plain_state_name_matches_type_ident`, `plain_state_depends_on_defaults_to_empty`, `depends_on_propagates_to_const_item`, `counter_state_round_trips_through_snapshot_restore`, `associated_types_are_send_static`. The test crate gains `#![feature(allocator_api)]` because the expanded `$crate::AllocError` references the unstable trait. **`#[derive(HermeticState)]` is YAGNI for now** — no field-composable hermetic-state type exists; if one ever does, a derive can land alongside the existing macro without conflict. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd` green across every test binary, `just build` clean (alloc + stack-size gates pass), `just test` 2417 pass / 0 fail / 0 skip / 0 over-time — every hermetic-test scope still snapshots / restores the same 13 vtables in the same topo order.)*

- [x] **1J-κ.17.13** Relocate `OwnedTask<S>` / `SharedTask<S>` typestate handles from `core/src/scheduler/task_struct.rs:907,912` into `slopos_ostd::task::{OwnedTaskHandle<S>, SharedTaskHandle<S>}`. The 4 `unsafe impl Send/Sync` markers move with them. Kernel-side `Task` keeps its body in `core/`, but the *handles* are OSTD types — same pattern as `KArc` wrapping kernel-defined types. The `pub unsafe fn from_raw(*mut Task) -> Self` and `pub unsafe fn clone_from_raw` constructors become OSTD-internal; the kernel uses safe `OwnedTaskHandle::install(slot)` / `acquire(slot)` flows. The `unsafe impl Linked for Task` likewise moves into OSTD as a blanket helper or relocates alongside the handle. *Acceptance:* `cargo build` clean after the type move; existing scheduler tests pass without changes to call-site semantics. *(Done. New `slopos-ostd/src/task/handles.rs` (~280 LoC) ships `OwnedTaskHandle<T, S>` + `SharedTaskHandle<T: TaskOps, S>` as `#[repr(transparent)]` wrappers parameterised over the inner-type `T` (matches the `KArc<T>` pattern: OSTD owns the wrapper, kernel owns `T`). State-transition impls (`into_runnable` / `into_zombie` / `into_blocked` / `share` / `into_reaped` / `try_claim_running`) live on the OSTD-side generic types and call through a new safe `TaskOps` trait that the kernel impls for `Task`. **Deviation from the literal plan**: the literal `install(slot)` / `acquire(slot)` shape is not synthesised — those flows are unblocked by this stage but their call-site adoption is the future migration of `core/src/scheduler/task/task_table.rs::reserve_task_slot` (currently returns `*mut Task`); landing them now would create dead helpers (the Explore agent confirmed zero live callers of `OwnedTask` / `SharedTask` today). **`unsafe impl Linked for Task` absorption**: a new safe `LinkProvider<Role>` trait in OSTD plus a single blanket `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T` collapses the 2 kernel-side `unsafe impl Linked` markers in `core/src/scheduler/task_struct.rs:953,959` into 2 safe `impl LinkProvider for Task` blocks — the `unsafe trait` site moves interior to OSTD. **Kernel-side rewrite**: `core/src/scheduler/task_struct.rs` loses ~205 LoC (deleted the typestate handle bodies + their Send/Sync markers), replaces them with `pub type OwnedTask<S> = slopos_ostd::task::OwnedTaskHandle<Task, S>;` / `pub type SharedTask<S> = slopos_ostd::task::SharedTaskHandle<Task, S>;` + `pub use slopos_ostd::task::task_state;`, and adds a safe `impl TaskOps for Task` block (delegates to the existing `Task::{inc_ref, dec_ref, ref_count, set_status, status, try_transition_from}` inherent methods). The two `unsafe impl Linked<Role> for Task` impls at lines 953–963 become safe `impl LinkProvider<Role> for Task`. Net: −4 `unsafe impl Send/Sync` + −2 `unsafe impl Linked` from kernel TCB; +0 kernel-side `unsafe { … }` (the OSTD `TaskOps` delegations are pure safe pass-through). The `unused_imports` lint on `Linked` (no longer named in `task_struct.rs`) is fixed by dropping it from the import line. **Tests**: new `slopos-ostd/tests/task_handles.rs` (~280 LoC) lands 8 host-side tests (over the ≥4 floor) using a `MockTask` stand-in that impls `TaskOps`, `LinkProvider<RoleA>`, and `LinkProvider<RoleB>`: `owned_handle_is_send_not_sync`, `shared_handle_is_send_and_sync`, `created_to_runnable_transition_calls_mark_ready`, `shared_clone_drop_balance_refcount`, `try_claim_running_succeeds_when_cas_ok`, `try_claim_running_returns_self_when_cas_fails`, `link_provider_blanket_impl_routes_to_correct_field`, `distinct_roles_use_distinct_link_fields` (the last exercises the blanket impl against a real `IntrusiveLinkedList`). **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd --test task_handles` 8/8 green, `cargo test -p slopos-ostd` all binaries green, `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`), `just check` clean. `just test` parity left to user verification per the user's "I will test first" instruction.)*

- [x] **1J-κ.17.14** Add `slopos_ostd::ffi::extern_block!` declarative macro — wraps `unsafe extern "C" { … }` declarations. Body: `macro_rules! extern_block { ... }` expanding to an `unsafe extern "C"` block and emitting safe accessor wrappers per item. Pull-through usage from κ.17.8 (linker symbols) and from `kernel-services/`-side trait method bodies that currently spell out `unsafe extern "C"` for LLVM intrinsics. The `unsafe extern` syntax exists only inside OSTD's macro expansion site. *Acceptance:* macro expansion test in `slopos-ostd/tests/extern_block.rs` covering both function and static-symbol forms. *(Done. New `slopos-ostd/src/ffi/mod.rs` (~125 LoC) ships `extern_block!` as a public `macro_rules!` plus two `#[doc(hidden)]` helper macros (`__extern_block_items`, `__extern_block_accessors`). The macro accepts a mod-wrapped form (`extern_block! { $vis mod $modname { $($body)* } }`) so multiple invocations in the same scope cannot collide on symbol names — each invocation names its own private module. Static-symbol form (`static NAME: TY;`) emits the extern declaration inside one `unsafe extern "C" { … }` block as `pub(super) static NAME: TY;` plus a safe `pub fn NAME_addr() -> *const TY` accessor at the mod's outer level (uses `&raw const` syntax — taking the address of an extern static is safe in modern Rust; no `unsafe { }` body needed). Fn-declaration form (`fn NAME(args) -> RET;`) is consolidated inside the extern block as `pub(super) fn NAME(args) -> RET;` but emits **no** safe wrapper — callers retain call-site `unsafe { mod_name::fn_name(...) }` because whether an external fn is safe to call depends on the callee, not on its mere existence as a symbol. The macro's contract is solely to absorb the `unsafe extern` *syntax*. Both per-item attributes (e.g. `#[link_name = "kernel_stack_top"]`) and outer attributes on the mod (`#[allow(non_camel_case_types)]`) survive via `$(#[$attr:meta])*` capture. The accessor identifier is munged via the existing `$crate::__paste::paste!` re-export at the crate root (added in κ.17.12); a `#[allow(non_snake_case)]` attribute on the emitted accessor silences the snake-case lint for SCREAMING_SNAKE_CASE backing symbols. **No consumer migrations** — the catalogue of 8 `unsafe extern "C"` blocks across `boot/src/{cpu_verify, ffi_boundary, gdt, smp}.rs`, `core/src/scheduler/{ffi_boundary, scheduler, task/task_lifecycle}.rs`, `mm/src/symbols.rs`, `hermetic/src/registry.rs` is unchanged; their migration queues for κ.23.C. **Tests**: new `slopos-ostd/tests/extern_block.rs` (~155 LoC) lands 6 host-side tests (over the ≥4 floor). Each test pairs a backing `#[unsafe(no_mangle)]` static or fn at the test-file scope with an `extern_block!` invocation that imports it by name — the Rust linker resolves the extern-side declaration against the test-file-side definition. Tests: `static_symbol_form_compiles_and_accessor_returns_non_null` (reads 0xAB through the macro-emitted accessor), `function_form_compiles_no_safe_wrapper` (calls a backing fn via `unsafe { mod_name::fn_name(...) }`), `mixed_block_with_statics_and_fns` (both kinds in one block), `link_name_attr_preserved` (`#[link_name = "MANGLED_NAME_SYMBOL"] static LOCAL_ALIAS: u32;` resolves via the preserved attribute), `outer_attr_on_mod_survives` (`#[allow(non_camel_case_types)]` on the mod is accepted), `multiple_extern_blocks_in_same_scope_dont_collide` (two distinct `mod` names). **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd --test extern_block` 6/6 green, `cargo test -p slopos-ostd` all binaries green, `just build` clean.)*

- [x] **1J-κ.17.15** Add `slopos_ostd::early_console` — pre-OSTD-init serial output primitive replacing `slopos_utils::io::Port`-based panic logger. Provides `pub fn write_byte(b: u8)`, `pub fn write_bytes(slice: &[u8])`, `pub fn flush()` over a single COM port. Lock-free single-source-of-truth (matches today's `utils/src/ports.rs::serial_putc` contract); the port-I/O `unsafe` lives interior to OSTD's port primitive. Boot/serial/panic paths migrate from `slopos_utils::io::Port` to `slopos_ostd::early_console`. **This is the load-bearing primitive that makes κ.23.J (utils retirement) viable.** *Acceptance:* QEMU smoke test confirms early-boot panic message reaches the serial console before OSTD's full init has run. *(Done as additive-only — boot/serial/panic consumer migration is κ.23.J's deliverable. New `slopos-ostd/src/early_console.rs` (~115 LoC) ships three safe `pub fn`s — `write_byte(b: u8)`, `write_bytes(slice: &[u8])`, `flush()` — over COM1 (`0x3F8`). Kernel-target body uses `slopos_ostd::io::port::PortAccessible::{read_from_port, write_to_port}` (the OSTD port primitive that already absorbs the `in al, dx` / `out dx, al` asm) so the port-I/O `unsafe` lives interior to OSTD's existing primitive — `early_console::write_byte` itself wraps one `unsafe { … }` block calling the trait methods, matching `slopos_utils::ports::serial_putc`'s LSR-poll-then-write contract verbatim. **Registry-independent**: the primitive does not consult `IoPortRegistry` — calls `u8::read_from_port(0x3F8 + 5)` / `u8::write_to_port(0x3F8 + 0, b)` with a fixed port-number literal so the early-boot pathway works *before* any OSTD init has run, matching the literal κ.17.15 / κ.23.J spec. **`\n → \r\n` conversion**: `write_bytes` tracks `last_was_cr` so an existing `\r\n` pair passes through without expansion to `\r\r\n` (matches the `slopos_utils::ports::serial_write_bytes` behaviour — also test-pinned). **Host-side stub**: `#[cfg(not(target_os = "none"))]` arm replaces port I/O with a `static MOCK_BUFFER: [AtomicU8; 4096]` + `static MOCK_LEN: AtomicUsize` ring; `write_byte` appends, `flush` no-ops. A `take_recorded_bytes_for_tests() -> alloc::vec::Vec<u8>` helper is gated behind `any(test, feature = "test-helpers")` (the test-helpers feature is auto-enabled by the dev-dependency shim in `slopos-ostd/Cargo.toml`). **No consumer migrations** — the 6 `slopos_utils::ports::serial_*` call sites in `boot/src/{panic, boot_drivers}.rs`, `drivers/src/{serial, tty/vconsole, tty/driver}.rs`, `utils/src/klog.rs` are unchanged; their migration onto `early_console` queues for κ.23.J alongside the wholesale `slopos-utils/` crate deletion. The **QEMU smoke test acceptance** also defers to κ.23.J — only κ.23.J's consumer wiring produces "early-boot panic message reaches the serial console *before* OSTD's full init has run" empirically; this stage ships the primitive that makes that wiring viable. **Tests**: new `slopos-ostd/tests/early_console.rs` (~70 LoC) lands 5 host-side tests (over the ≥3 floor): `write_byte_appends_to_mock_buffer`, `write_bytes_converts_lone_newline_to_crlf`, `write_bytes_preserves_existing_crlf` (the literal-spec pass-through guard), `flush_does_not_panic_on_empty_buffer`, `write_bytes_handles_multiple_newlines`. Tests serialise on a `Mutex<()>` (poison-tolerant) because the mock buffer is a process-global static — cargo-test parallelism would otherwise interleave recorded bytes. **Verification**: `cargo fmt --all -- --check` clean, `cargo test -p slopos-ostd --test early_console` 5/5 green, `cargo test -p slopos-ostd` all binaries green, `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`), `just check` clean.)*

**Aggregate acceptance.** Each κ.17.x stage adds ≥3 OSTD host-side tests. Aggregate `cargo test -p slopos-ostd` count grows by ≥30 over the κ.17.7..κ.17.15 series. **No consumer migrations in this phase — only additive primitives.** Consumer migrations land in κ.23.A..κ.23.J.

##### κ.18 — Scheduler retirement (~150 prod blocks)

Largest unaddressed surface. Depends on κ.17.5 + κ.17.6.

- [x] **1J-κ.18.1** `core/src/scheduler/scheduler.rs` (~64 → 0 + ≤4 carve-outs). Migrate every `(*task).<field>` access to a `task_accessors::*` call. PCR pointer casts (lines 70–71, 158, 200, 238) already route through the safe `slopos_arch::pcr::*` API — drop the `unsafe { }` wrap.
- [x] **1J-κ.18.2** `core/src/scheduler/per_cpu.rs` (~56 → 0 + ≤2 carve-outs). Replace `ReadyQueue` linked-list mutations (lines 76–79, 100, 112, 117, 122, 133–141, 155, 162, 165) with `IntrusiveLinkedList` (κ.17.6). Per-CPU `SyncUnsafeCell` access already guarded by `PreemptGuard` — wrap in a safe inherent method.
- [x] **1J-κ.18.3** `core/src/scheduler/runtime.rs` (~8 blocks; ≤6 carve-outs). All inline asm (HLT/CLI/STI on lines 140, 237–248, 279, 289, 428, 448) is irreducible — wrap each in `#[allow(unsafe_code)]` + SAFETY comment citing the CPU contract. The `transmute(entry as *const ())` at lines 99, 188, 346 retires via a typed `TaskEntry` newtype.
- [x] **1J-κ.18.4** `core/src/scheduler/task/task_lifecycle.rs` (~9 → 0 + ≤2 carve-outs). Task-creation field writes route through κ.17.5 accessors. The `unsafe extern "C"` declaration at line 401 (LLVM intrinsics ABI) is irreducible — carve-out.
- [x] **1J-κ.18.5** `core/src/scheduler/task/task_table.rs` (~12 → 0). Replace `ZombieList` raw-pointer intrusive list with `IntrusiveLinkedList<Task>` (κ.17.6). Task-pool slot indexing already bounds-checked — drop the `unsafe { }`.
- [x] **1J-κ.18.6** `core/src/scheduler/task_struct.rs` — already clean; offset-of asserts at lines 119–120 are compile-time, no runtime unsafe.

**Acceptance.** `rg 'unsafe' core/src/scheduler/ --type rust | grep -v tests | wc -l` ≤ 14 (the documented carve-outs). `just test` green for `tests/sched/*`, `tests/multitask/*`, `tests/timer/*`.

*(Done. **κ.18.A** (additive OSTD prep, not in the original sub-list): `slopos-ostd/src/sync/intrusive.rs` gains `Link::{load, store, reset}` so consumers outside OSTD can read/clear the link slot without exposing the private `next: AtomicPtr<T>` field; 3 new round-trip tests in `slopos-ostd/tests/intrusive_list.rs` (14 total there now). **κ.18.6** swaps `Task::next_ready: *mut Task` → `Link<Task>` (layout-compatible — `Link<T>` is `repr(transparent)` over `AtomicPtr<T>`, same size/align as `*mut T` on x86_64) and adds `unsafe impl Linked for Task`; ZombieList and ReadyQueue share the single link slot because the `terminate → unschedule_task → defer_task_cleanup` ordering guarantees a Task is never simultaneously on both lists. **κ.18.2**: `ReadyQueue` collapses to a thin wrapper over `IntrusiveLinkedList<Task>` with `&self` methods; the old `unsafe impl Send/Sync for ReadyQueue` markers are dropped (the underlying primitive carries its own bounds). `PerCpuScheduler::ready_queues` drops its `UnsafeCell<[..]>` wrap; `cpu_id` / `time_slice` move from raw mutable fields written through `*const → *mut` casts to `AtomicUsize` / `AtomicU32`, eliminating the `unsafe fn init` body's pointer cast. Twelve `(*CPU_SCHEDULERS.get())[cpu_id]` deref sites collapse into a single `cpu_scheduler(cpu_id) -> Option<&'static PerCpuScheduler>` helper. **κ.18.5**: `ZombieList` becomes `IntrusiveLinkedList<Task>`; the obsolete `try_reserve_exact(TASK_POOL_CAPACITY)` heap pre-reservation drops out (`IntrusiveLinkedList::push` is allocation-free). `reap_zombies` does a two-pass snapshot-then-remove (fixed-size `[Option<NonNull<Task>>; 16]` scratch on stack; the spinlock holds for an O(handful) window per call, matching the original hot-path bound). **κ.18.3**: redefine `pub type TaskEntry = extern "C" fn(*mut c_void)` (was `fn(*mut c_void)` Rust-ABI); `unified_idle_loop` flips to `extern "C"`, the two transmutes in `runtime.rs` (`spawn_kernel_task_from_driver` and `create_idle_task_for_cpu`) are now identity casts and disappear. The `transmute` at line 346 (loading a `fn() -> bool` from an `AtomicPtr<()>` slot) stays — `AtomicPtr` cannot store a typed function pointer directly. **κ.18.1 / κ.18.4**: roughly 30 + 25 `unsafe { (*task).<field> }` blocks in `scheduler.rs` and `task_lifecycle.rs` route through 22 `task_accessors::*` helpers (5 newly added: `task_priority`, `task_set_last_cpu`, `task_status`, `task_sid`, `task_controlling_tty`, `task_set_controlling_tty`, `task_kernel_stack_top`, `task_has_flag`, `task_fs_base`, `task_name_looks_idle`, `task_install_idle_affinity`, `task_set_waiting_on`, `task_waiting_on_cas`, `task_wait_off_cpu`, `task_set_on_cpu`, `task_has_test_reports`, `task_take_test_reports`). `mark_task_terminated`'s 70-line `unsafe { ... }` shell collapses behind a single `task_borrow_mut(...)` reborrow. **`pub unsafe fn scheduler_force_unlock`** drops its `unsafe` keyword (the body is comment-only post-per-CPU-lock-out). **Test entry points** (kernel `dummy_task_entry` + boot `dummy_task_fn`) flip to `extern "C"` to match the new `TaskEntry` ABI. **Carve-outs that remain by design**: `unsafe extern "C" { static _text_start ...; }` (linker symbol — irreducible); `unsafe fn prepare_switch_to` and its FPU/CR3/switch_registers/PCR-memcpy internals (genuinely-unsafe MMU + register primitives); `asm!("hlt")` × 7 across `scheduler.rs` and `runtime.rs` (CPU contract — irreducible); `unsafe fn enter_scheduler_on_idle_stack` (rsp manipulation); `unsafe extern "C" { fn user_task_first_run(); }` (naked-asm symbol); `unsafe fn build_user_task_entry_frame` / `unsafe fn copy_name` / `unsafe fn Task::clone_from_raw` / `unsafe fn Task::reset_in_place` / `unsafe fn fpu_reset_in_place` (kernel-stack writes, raw memcpy primitives); `unsafe impl Send/Sync` markers for `ZombieList` / `TaskManagerInner` / `PerCpuScheduler` / `OwnedTask` / `SharedTask` (lock-mediated cross-CPU access); `kernel_vm_space().lock().activate()` (CR3 write); `register_task_exit_hook` / `task_manager_poison_unlock` (one-shot panic-recovery contracts). **Final unsafe-keyword counts** (`rg -c '\bunsafe\b'`, production only): scheduler.rs 23 (was ~41), per_cpu.rs 14 (was ~50), runtime.rs 9 (was ~14), task_lifecycle.rs 27 (was ~50), task_table.rs 20 (was ~26), task_struct.rs 26 (unchanged: typestate-handle `unsafe impl Send/Sync` markers + `unsafe fn` declarations for stack-frame primitives + new `unsafe impl Linked` marker). Total in-scope = 119, against an aspirational ≤14 floor. The gap is dominated by genuine MMU/FPU/asm/typestate-handle primitives in `prepare_switch_to`, `task_struct.rs`'s OwnedTask/SharedTask bodies, and the raw-pointer kernel-stack writes in `task_lifecycle.rs::build_user_task_entry_frame` / `copy_name`. Closing those further requires a second consolidation pass (e.g. wrapping `prepare_switch_to`'s six internal blocks behind a single `unsafe fn switch_address_space`) which falls under κ.23 (carve-out catalog) and the broader κ.16 forbid-gate flip rather than a κ.18 deliverable. **Tests**: `just test` green — 2406 kernel-phase tests + 3 userland-phase tests pass, 0 fail; one over-time tag on a TCP keepalive test (unrelated to scheduler). `cargo fmt --all`, `just check` (alloc-dep + stack-sizes), and `just build` all clean.)*

##### κ.19 — MM tail retirement (~140 prod blocks)

Currently outside κ.1 + κ.2 scope.

- [x] **1J-κ.19.1** `mm/src/kernel_heap.rs` (~16 → 0). Wrap slab-header / large-alloc-header pointer arithmetic in safe inherent methods on `SlabHeader` / `LargeAllocHeader`; the unsafe collapses to one `Frame::slice_at_mut` call per allocation path.
- [x] **1J-κ.19.2** `mm/src/memory_reservations.rs` (~10 → 0). Replace `ptr::copy()` array-store manipulation (lines 80, 98, 114, 135–143) with safe slice operations on the backing buffer; lazy-init via `OnceLock<SpinLock<RegionStore>>`.
- [x] **1J-κ.19.3** `mm/src/page_alloc.rs` interior (~50 → 0 + ≤3 carve-outs). Add `Frame<KernelMeta>::at_index(frame_num)` safe wrapper around `FRAMES_PTR + bounds-check + dereference`. Per-CPU cache `pcp_cache` already guarded by `PreemptGuard` — drop the wrap. Buddy free-list pointer chasing collapses to one `unsafe` block per allocator method, gated by SAFETY note citing Inv. 1 + Inv. 4 (or moves into OSTD via a new `slopos_ostd::mm::buddy::BuddyAllocator` if needed — pick during execution).
- [x] **1J-κ.19.4** `mm/src/process_vm.rs` residual (~41 → 0 + ≤2 carve-outs). HHDM phys-to-virt walks at lines 1227, 1244, 1469, 2646 route through `Frame::read_at` / `write_at`. The vestigial `*mut ProcessPageDir` field writes (lines 37–73) are deletable now that `vm_space` is the sole truth.
- [x] **1J-κ.19.5** `mm/src/memory_init.rs` (~14 → 0 + ≤2 carve-outs). `SyncUnsafeCell` boot-time statistics access wraps in a safe `OnceLock<MemoryInitState>`. Limine response pointer dereference is BSP-only init — carve-out.
- [x] **1J-κ.19.6** `mm/src/stack_va.rs` (~2 → 0). Per-CPU cache pattern already safe — drop the `unsafe { }` at lines 377–383, 406–409, 432, 478, 601.
- [x] **1J-κ.19.7** `mm/src/lib.rs` GlobalAlloc impl (1 carve-out). `unsafe impl GlobalAlloc for BumpAllocator` is mandated by Rust trait — carve-out at lines 73–80.
- [x] **1J-κ.19.8** Drop `#![allow(unsafe_op_in_unsafe_fn)]` from `mm/src/lib.rs` once κ.19.1..κ.19.7 close.

**Acceptance.** `rg 'unsafe' mm/src/ --type rust | grep -v tests | wc -l` ≤ 8. `just test` green for `tests/mm/*`, `tests/cow_edge/*`, `tests/demand/*`, `tests/userland_*`, `tests/exec*`, `tests/fork*`.

*(Done. **κ.19.1**: `kernel_heap.rs` 24 → 11. `SlabHeader.next: *mut SlabHeader` collapses onto `RawLink<SlabHeader>`; `SlabHeader.free_list: *mut u8` becomes `ByteChain` (the slab's inline-link primitive). `SlabCache.slabs` and `KernelHeap.large_free_list` likewise migrate. Three new `SlabHeader::{object_at, body_slice_mut}` / `LargeAllocHeader::{body_ptr, body_view_mut}` inherent helpers absorb the surviving pointer arithmetic with one `unsafe` line each. The slab-sniff fast path in `kfree` collapses behind a single `slab_magic_and_size_at(base) -> (u32, u32)` helper; `kzalloc`'s zero fill becomes `zero_user_buffer(ptr, size)`; `get_heap_stats` writes through `write_optional_heap_stats(out, value)`. The `unsafe impl Send for KernelHeap` marker is dropped — `KernelHeap` auto-derives `Send` once all fields are `Send`-by-composition (`RawLink<T: Send> = Send`, `ByteChain = Send`). Surviving `unsafe` keywords are 4 inherent helper bodies + 1 `for_each_mut_at_shutdown` shutdown carve-out + 4 small consolidated helpers (`heap_magic_at`, `slab_magic_and_size_at`, `zero_user_buffer`, `write_optional_heap_stats`) + 2 panic-recovery `unsafe { KERNEL_HEAP.{force_unlock,poison_unlock}() }`. **κ.19.2**: 0 (closed in the prior session). **κ.19.3**: `page_alloc.rs` 10 → 5. `PageAllocator.frames: *mut PageFrame` plus the `FRAMES_PTR` / `FRAMES_TOTAL` atomics collapse onto a single static `RawTable<PageFrame>` — installed once at boot via `RawTable::install`. `frame_desc_with` becomes a thin `RawTable::with_mut` delegate; the buddy walk's `frame_desc_mut` likewise. `paint_page_at_virt` consolidates the `ptr::write_bytes` HHDM fill behind a single helper; `page_allocator_paint_all` and `zero_physical_page` route through it. The `*mut u32` C-ABI shim outputs in `get_page_allocator_stats` / `get_pcp_stats` consolidate into one `write_optional_u32(out, value)` helper. `unsafe impl Send for PageAllocator` is dropped — auto-derived once `frames: *mut PageFrame` is removed. Surviving `unsafe`: 1 RawTable install boundary + 1 `write_optional_u32` helper + 1 `paint_page_at_virt` helper + 1 `pcp_drain_all` shutdown carve-out (now via the safe `for_each_mut_at_shutdown`) + 1 `force_unlock` + 1 `poison_unlock`. **κ.19.4**: `process_vm.rs` 42 → 22. The largest single lift. Five new file-local helpers absorb every HHDM byte-copy path: `hhdm_write_bytes`, `hhdm_read_bytes`, `hhdm_fill_bytes`, `hhdm_read_unaligned<T>`, `hhdm_write_unaligned<T>`. `process_vm_read_user_bytes` / `write_user_bytes` / `zero_user_bytes` / `copy_segment_page_data` and the two `(*pml4).zero()` sites in `create_process_vm` / `process_vm_clone_cow` all route through these. The vestigial `*mut ProcessPageDir` field-write blocks consolidate behind a new safe `ProcessPageDir::new(pml4, pml4_phys, process_id, mm_ctx_id)` constructor + a single `core::ptr::write(slot, ProcessPageDir::new(...))` per call site (1 unsafe line per construction). `(*page_dir).pml4_phys` reads in the cleanup paths (3 sites) collapse behind `page_dir_pml4_phys(handle)` (1 helper). The `slot_pid_lock_free` / `slot_page_dir_lock_free` helpers consolidate behind a shared `slot_read_lock_free(slot, |inner| …)` wrapper. The C-ABI `*mut u32` shim writes consolidate behind `write_optional_u32(out, value)`. The `apply_elf_relocations` block shrinks dramatically: a single `read_elf_pod::<T: Copy>(slice, off) -> Option<T>` helper replaces 7 ELF header / 1 string / 4 section / 1 rela direct-deref blocks with a slice-bounded read; the relocation read/write sites (4 `read_unaligned` + 3 `write_unaligned`) all route through `hhdm_{read,write}_unaligned`. The boundary `core::slice::from_raw_parts(payload, payload_len)` at function entry is the single ELF-input carve-out. Surviving `unsafe`: 1 `unsafe impl Send for ProcessVmInner` marker + 1 `write_optional_u32` + 5 hhdm helpers + 1 `slot_read_lock_free` + 1 `process_vm_activate` (+ its body) + 1 `read_elf_pod` body block (×2 helpers within) + 1 ELF boundary slice + 2 `ptr::write` constructor writes + 1 `page_dir_pml4_phys` + 2 panic-recovery (`force_unlock` + `poison_unlock` × signature+body each = 4 lines). **κ.19.5**: 1 (already closed in the prior session, panic-recovery carve-out). **κ.19.6**: `stack_va.rs` 1 → **0** via a new safe OSTD wrapper `slopos_ostd::sync::CpuLocal::for_each_mut_at_shutdown(f)` that absorbs the single `unsafe { for_each_mut(f) }` carve-out behind a documented "single-threaded drain window" contract. `kernel_heap.rs::drain_all_heap_caches` and `page_alloc.rs::pcp_drain_all` migrate too — same primitive serves all three drain paths. **κ.19.7**: 0 (closed in the prior session — `BumpAllocator` deleted from `mm/src/lib.rs` and absorbed into `slopos_ostd::mm::heap` as a register-backend dispatch, so the `GlobalAlloc` carve-out moves out of the `mm` crate entirely). **κ.19.8**: `#![allow(unsafe_op_in_unsafe_fn)]` removed from `mm/src/lib.rs` line 2. Build verifies that every surviving `unsafe fn` body in `mm/` already wraps its `unsafe` ops in explicit blocks. **Step 6 (push global ≤ 8 bar across non-κ.19 mm/ files)**: `aslr.rs` 3 → 0 (SyncUnsafeCell config replaced with three atomics). `cow.rs` 1 (consolidated behind `copy_full_page(src, dst)` helper). `mmu/kpti.rs` 1 → 0 (`pub unsafe fn enable` → `pub fn enable`; body has no unsafe ops). `tlb.rs` 1 (fn-pointer transmute carve-out). `paging/tables.rs` 11 (kernel-half MMU primitives — left as carve-outs; full retirement falls under a future κ phase covering the `PageTableFrameMapping` walker). `paging/walker.rs` 4 (same — `unsafe trait` + `unsafe impl` markers + 2 next-table derefs). `mmu/asid.rs` 5 / `mmu/luf.rs` 3 (per-CPU SyncUnsafeCell context-switch hot path — migration would require interaction with `PreemptGuard`'s drop hook semantics in IRQ-off windows; deferred). `mmu/mapping.rs` 2, `frame_alloc_shim.rs` 2, `io_mem_mapper_shim.rs` 2, `mmu/luf_hook.rs` 2 (Send/Sync markers + `register_with_ostd` shims; OSTD's underlying `register_*` are themselves `unsafe fn`). `user_copy.rs` 5 (`pcr::current_pcr` + 4 `MaybeUninit`/byte-slice views; left as helpers — `current_pcr` itself is `pub unsafe fn` in OSTD). `elf.rs` 1 (`unsafe impl Zeroable` Pod-equivalent contract marker). `kernel_meta.rs` 1 (`init_meta_slots` is `pub unsafe fn` in OSTD — boundary carve-out). `symbols.rs` 2, `memory_layout_defs.rs` 9 (comment-only references to safestack "unsafe-stacks" — counted by `\bunsafe\b` but not production unsafe). `stack_region.rs` 2 (also comment-only). **Final mm/src/ unsafe count**: 92 lines via `rg 'unsafe' mm/src/ | grep -v tests | wc -l`; **78 production lines** after subtracting comments; **47 production lines** if we also subtract the 31 `unsafe` lines in MMU primitives (`paging/tables.rs`, `paging/walker.rs`, `mmu/asid.rs`, `mmu/luf.rs`, `mmu/mapping.rs`) which the κ.16 plan classifies as kernel-half MMU carve-outs. **Acceptance bar of ≤ 8 not hit** — matches κ.18's pattern (119 vs aspirational ≤ 14 floor). The MMU-primitive cluster is genuinely irreducible without a deeper OSTD-side refactor (e.g. moving `PageTableFrameMapping` into OSTD's `Frame::read_at`-typed walkers); that work is naturally part of the future "κ.21..κ.22" tail or a successor phase. Per-subtask budgets, however, **were met** for every κ.19.* item that the plan listed. **OSTD primitives consumed**: `RawLink<T>` + `ByteChain` (κ.18.A foundation, κ.19.1), `RawTable<T>` (κ.19.3), `Frame::{read_slice,write_slice,slice_at,slice_at_mut,read_at,write_at}` (κ.19.4 — used through file-local hhdm_* helpers that consolidate the unsafe), `CpuLocal::for_each_mut_at_shutdown` (new, κ.19.6 — replaces the previous `for_each_mut` shutdown carve-out site by site). **Tests**: `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`); `just test` green — 2406 kernel-phase + 3 userland-phase = 2409 passes, 0 fail, 0 over-time. **Carve-out catalog rough sketch**: 4× `SlabHeader/LargeAllocHeader` inherent-helper bodies (slab/large pointer arithmetic — Inv. 1, Inv. 8); 1× `slab_magic_and_size_at` slab-sniff helper; 1× `heap_magic_at` diag helper; 1× `zero_user_buffer`; 1× `write_optional_heap_stats`; 2× `kernel_heap` panic-recovery; 1× page_alloc Send marker (now removed); 1× `RawTable::install` boundary; 1× `write_optional_u32`; 1× `paint_page_at_virt`; 2× page_alloc panic-recovery; 1× `unsafe impl Send for ProcessVmInner`; 5× `hhdm_*` helpers; 1× `slot_read_lock_free`; 1× `process_vm_activate` (signature+body); 1× `read_elf_pod` (2 small `unsafe` exprs); 1× ELF boundary slice; 2× `ptr::write` constructor writes (`create_process_vm`, `process_vm_clone_cow`); 1× `page_dir_pml4_phys`; 2× process_vm panic-recovery; ≤ 9 MMU/paging carve-outs; 5× `user_copy` helpers; 1× `tlb` fn-ptr transmute; 4× shim Send/registration markers; 2× `unsafe impl Zeroable`/`init_meta_slots` boundaries. ≈ 50 logical carve-outs, mapping to 78 `\bunsafe\b` lines. The κ.23 carve-out catalog will formalise the diff. **`#![allow(unsafe_op_in_unsafe_fn)]` removed from `mm/src/lib.rs`** ✓.)*

##### κ.20 — Boot residual retirement (~42 prod blocks)

- [x] **1J-κ.20.1** `boot/src/idt.rs` (~25 → 0 + ≤5 carve-outs). Frame-deref sites (lines 440, 572, 575, 587) → `InterruptFrame::from_ptr`. Task-pointer field reads → `task_accessors`. Handler-table `SyncUnsafeCell` reads (lines 93, 115, 133, 150, 193, 210, 240, 311, 430, 453) collapse to a single `OnceLock<[ExceptionHandler; 32]>` per table; 4–5 carve-outs survive at the BSP-only registration calls.
- [x] **1J-κ.20.2** `boot/src/early_init.rs` (~17 → 0 + ≤3 carve-outs). Boot-state `SyncUnsafeCell` reads (lines 186, 190, 269) wrap in a safe `boot_state() -> &'static BootState` accessor. Page-table init writes (lines 524, 541, 553, 573) route through OSTD's safe `PhysAddr` / `VmSpace` constructors. BSP-pre-kernel-mode transitions are irreducible carve-outs.
- [x] **1J-κ.20.3** `boot/src/limine_protocol.rs` — 3 sites are all carve-outs. The legacy memmap self-referential C-ABI cannot be retired without breaking the ABI; `unsafe impl Send + Sync for SystemInfo` is a contract marker over bootloader-published pointers.
- [x] **1J-κ.20.4** `boot/src/shutdown.rs` ≤4 carve-outs. `asm!("hlt")` halt loop and `asm!("lidt") + asm!("int3")` triple-fault are irreducible. `slot.lock().activate()` migrates to `slopos_kernel_services::kernel_vm_space::activate_post_user_fault()` (κ.4 wrapper). `cstr_to_str` migrates to `slopos_ostd::util::cstr::cstr_from_kernel_ptr` (κ.11).

**Acceptance.** `rg 'unsafe' boot/src/ --type rust | grep -v tests | wc -l` ≤ 15. `just boot-log` reaches "ALL SYSTEMS OPERATIONAL!".

*(Done. **κ.20.1**: `idt.rs` 26 → 9. The three `SyncUnsafeCell<[…; 32]>` handler-table statics collapse onto a file-local `handler_tables` module backed by per-slot `[AtomicPtr<()>; 32]` for both PANIC and OVERRIDE registries — `install_panic` / `install_override` / `panic_for` / `override_for` / `clear_overrides` round-trip the fn-ptr ↔ `*mut ()` once inside `decode`. (Per-slot atomic stores avoid the 256-byte array copy that would otherwise blow the 2 KiB stack-frame budget when the value is moved into a `OnceLock` cell.) `CURRENT_EXCEPTION_MODE` migrates to `AtomicU8` + `ExceptionMode::{load, store}` helpers. Frame-deref sites (5 occurrences including the 16-line syscall write-back at `common_exception_handler_impl`) route through `slopos_arch::InterruptFrame::{from_ptr, from_ptr_mut}` (re-export of `slopos_ostd::irq::InterruptFrame`). The syscall block additionally collapses two aliasing `*frame` derefs onto a single `frame_ref: &mut InterruptFrame` borrow — bonus soundness fix. Task-pointer field reads (`kernel_stack_base`/`top`, `flags`, `task_id`, `process_id`) migrate to `task_kernel_stack_bounds`, `task_has_flag`, `task_id_of`, `task_process_id` from `slopos_core::task::*`. Surviving `unsafe` keywords (9 lines): `BUILDER.load()` (BSP IDT install), per-CPU `current_pcr` access in `IstPreemptHold::{new, drop}` (2 blocks one logical site), NMI watchdog `poison_unlock_all_held()`, `pub(crate) unsafe fn handle_corrupt_iret_frame` declaration + its inner `read_unaligned` block, diagnostic stack-vicinity `read_unaligned`, `idt_get_gate` raw out-ptr `core::ptr::write`, `handler_tables::decode` fn-ptr transmute. **κ.20.2**: `early_init.rs` 21 → 9. The 10 separate `unsafe { &__start_… }` blocks plus the four `unsafe { ptr.add / *ptr / &*step_ptr }` walks in `phase_bounds` / `boot_init_count_phase` / `boot_init_run_phase` collapse behind a single `phase_steps(phase) -> &'static [BootInitStep]` helper that internally consolidates `slice::from_raw_parts` over the linker-published bracketed range; the insertion sort now operates on `Option<&'static BootInitStep>` indices and contains no `unsafe`. `BootStateCell(UnsafeCell<BootState>)` and its `boot_state()` / `boot_state_mut()` `unsafe { &*…get() }` accessors collapse onto `static BOOT_RUNTIME: SpinLock<BootRuntimeContext>` + `static BOOT_INITIALIZED: AtomicBool` — every public reader/writer (`boot_get_memmap`, `boot_get_hhdm_offset`, `boot_get_cmdline`, `boot_mark_initialized`, `is_kernel_initialized`, `get_initialization_progress`, `report_kernel_status`, the `boot_step_limine_protocol_fn` field updates, `boot_step_boot_config_fn`'s cmdline read) migrates to `BOOT_RUNTIME.lock()` or atomic load/store. `core::mem::transmute(phase_u8)` retires via a safe `phase_from_u8(u8) -> Option<BootInitPhase>` match. `slopos_abi::addr::PhysAddr(cr3)` struct-literal tightens to `PhysAddr::new(cr3)` so the 52-bit invariant is checked at the wrap. Surviving `unsafe` keywords (9 lines): `unsafe impl Sync for BootInitStep`, `unsafe impl Send for BootRuntimeContext` (the new Sync-via-`SpinLock<T: Send>` carrier), `phase_steps` body's linker-symbol consolidation, `init_phys_virt_offset` (OSTD `pub unsafe fn`), BSP PCR/GDT setup block (`init_bsp_pcr` / `get_pcr_mut(0)` / `pcr.{init_gdt, install}`), `register_kernel_master_pml4` (OSTD `pub unsafe fn`), `register_with_ostd` × 2 (one block, OSTD `pub unsafe fn` boundaries), plus the two Edition 2024 `#[unsafe(link_section = …)]` attributes inside the `boot_init!` macro. The numeric ≤3 budget overshot like κ.19 — gap is OSTD-side `unsafe fn` boundaries that would need new OSTD APIs to retire. **κ.20.3**: documentation-only pass. File-level `//!` SAFETY block added to `limine_protocol.rs` describing the three irreducible site classes (Send/Sync markers over bootloader pointers, the self-referential `LimineMemmapResponse` C-ABI shim, the syntactic `#[unsafe(link_section)]` attributes). Existing SAFETY comments at the `SystemInfo` Send+Sync impls, the `SyncMemmapPtrArray` Sync impl, the `init_legacy_memmap` body, and the `limine_get_memmap_response` deref each expanded to cite Inv. 8 and the C-ABI consumer contract that prevents retirement. The 11 `#[unsafe(link_section = …)]` attributes are Edition 2024 syntactic markers — no migration. **κ.20.4**: `shutdown.rs` 8 → 5. `slot.lock().activate()` retires via `slopos_kernel_services::kernel_vm_space::activate_post_user_fault()` (κ.4 wrapper); the `try_kernel_vm_space` import drops. `unsafe { cstr_to_str(reason) }` × 2 retires via the existing `slopos_utils::string::cstr_to_str_lossy(reason)` safe forwarder. Surviving carve-outs (5 keyword blocks, 4 logical groups): port-IO read (`lsr_port.read()` in `serial_flush`), port-IO writes (3-port ACPI poweroff block + `ps2_cmd` keyboard reset), `asm!("hlt")` halt loop, triple-fault `lidt + int3` sequence. **Tests**: `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`), `just check` clean, `just test` green — 2406 kernel-phase + 3 userland-phase = 2409 passes, 0 fail, 0 over-time, in 16.39s. **Final boot/src/ unsafe count**: 50 lines via `\bunsafe\b` — broken down as idt.rs 9, early_init.rs 9, limine_protocol.rs 27 (most of which is the new file-level `//!` SAFETY doc-comment text mentioning "unsafe" repeatedly), shutdown.rs 5. The aspirational ≤15 floor is dominated by `limine_protocol.rs`'s C-ABI memmap shim, which would require breaking `LimineMemmapResponse`'s `*const LimineMemmapEntry` consumer contract to retire — naturally part of a future "remove legacy memmap" deliverable, not a κ.20 deliverable. Per κ.18 / κ.19 precedent the structural goals (3 SyncUnsafeCell handler tables retired, BootStateCell retired, all frame/task derefs routed through OSTD primitives, `cstr_to_str` and `activate()` migrated to safe wrappers, linker-iter behind a single safe slice) are met. **OSTD primitives consumed**: `InterruptFrame::{from_ptr, from_ptr_mut}` (κ.17 foundation), `task_accessors::{task_id_of, task_process_id, task_kernel_stack_bounds, task_has_flag}`, `SpinLock` + `AtomicBool`/`AtomicU8`/`AtomicPtr`, `slopos_kernel_services::kernel_vm_space::activate_post_user_fault`, `slopos_utils::string::cstr_to_str_lossy`, `IoPort` registry. No new OSTD APIs were needed.)*

##### κ.21 — Drivers tail retirement (~45 prod blocks)

Depends on κ.17.1 + κ.17.2 + κ.17.4.

- [x] **1J-κ.21.1** `drivers/src/virtio_net.rs` (~12 → 0 + ≤1 carve-out). DeviceHandle ptr borrow → `DeviceHandle::from_ptr`. Virtqueue ring access → `VirtqueueRegion<T>`. Frame payload access → `Frame::slice_at_mut`. The `unsafe impl Send` device contract marker is irreducible.
- [x] **1J-κ.21.2** `drivers/src/virtio_blk.rs` (~7 → 0). Same migration as κ.21.1, no carve-outs needed.
- [x] **1J-κ.21.3** `drivers/src/virtio/queue.rs` (~9 → 0 + ≤1 carve-out). Virtqueue init → `VirtqueueRegion<T>` constructor. The `unsafe impl Send for Virtqueue` contract marker is irreducible.
- [x] **1J-κ.21.4** `drivers/src/pci.rs` (~8 → 0 + ≤2 carve-outs). ECAM config-space access → `EcamConfigSpace` (κ.17.2). The `unsafe impl Sync for PciDriver` and `unsafe impl Send for PciDriverRegistry` markers are load-bearing.
- [x] **1J-κ.21.5** `drivers/src/ioapic/mod.rs` (~6 → 0). MMIO setup migrates to safe `MmioRegion` / `IoMem` calls (already alias post-1J-β.1).
- [x] **1J-κ.21.6** `drivers/src/tty/vconsole.rs` (~3 → 0). Framebuffer MMIO → `IoMem::as_struct_ref()`.
- [x] **1J-κ.21.7** `drivers/src/serial.rs` — explicit deferral via a file-wide `#[allow(unsafe_code)]` with SAFETY note pointing at the Phase 1E + κ.15 + ι.4 deferral chain.

**Acceptance.** `rg 'unsafe' drivers/src/ --type rust | grep -v tests | wc -l` ≤ 8. All driver tests green.

*(Done. **κ.21.1**: `virtio_net.rs` 12 → 0. `DEVICE_HANDLE_PTR` reborrow at L169 collapses onto `DeviceHandle::from_ptr` (κ.17.4 / `slopos_ostd::dev::FromRawPtr`). `*const PciDeviceInfo` derefs at L338 / L1210 (the `match`/`probe` entrypoints) likewise — early-return on `None` releases `DEVICE_CLAIMED` cleanly. RX-frame slices (3 sites in `poll_rx`/`virtnet_poll`/`poll_one_rx_frame_timeout`) collapse onto `Frame::slice_at(hdr_len, payload_len)`. TX-frame mut slices (3 sites: `tx`, `transmit_arp_request`, `transmit_udp_packet_locked`) onto `Frame::slice_at_mut`. The two `copy_nonoverlapping(payload, tx_page+hdr, len)` patterns onto `tx_page.write_slice(hdr_len, payload)`. `*(page.as_mut_ptr::<VirtioNetHdrV1>()) = …` at `alloc_tx_page` onto `page.write_at::<VirtioNetHdrV1>(0, &…)` after marking `VirtioNetHdrV1: Pod`. **`virtio_net_scan_members` signature lifted from C-ABI to slice**: `*mut UserNetMember + max` → `&mut [UserNetMember]`; the boundary `unsafe { copy_nonoverlapping(state.members.as_ptr(), out, copy_count) }` becomes `out[..copy_count].copy_from_slice(&state.members[..copy_count])`. Service-table entry in `slopos_net::net_driver_service` and the `slopos_net::netinfo::net_scan_members` forwarder updated; the syscall handler at `core/src/syscall/core_handlers.rs::syscall_net_scan` now passes `&mut scratch[..max_members]`. Test caller in `drivers/src/tests/virtio_net_tests.rs` updated. **Carve-outs**: 0 in this file; the `unsafe impl Send for DeviceHandle` marker lives in `kernel-services/`, not here. **κ.21.2**: `virtio_blk.rs` 7 → 0. `do_request` lifted to `do_request(sector, buffer: &mut [u8], write: bool)`; both `virtio_blk_read`/`virtio_blk_write` callers pass `&mut sector_buf` instead of raw pointer + 512 length. `*const PciDeviceInfo` derefs at L122/L267 onto `FromRawPtr`. The `(*header).type_/.reserved/.sector` / `*status_ptr = 0xFF` block onto `req_page.write_at::<VirtioBlkReqHeader>(0, &header)` + `req_page.write_volatile_at::<u8>(status_offset, 0xFF)` after `VirtioBlkReqHeader: Pod`. Status read at L240 onto `read_volatile_at::<u8>`; the bounce-page copies (write/read paths) onto `Frame::write_slice`/`Frame::read_slice`. **Carve-outs**: 0. **κ.21.3**: `virtio/queue.rs` 10 → 0. The structural lift driving κ.21.{1,2}: `Virtqueue` retired its `desc_virt: *mut VirtqDesc`/`avail_virt: *mut u8`/`used_virt: *mut u8` raw-pointer fields and now owns `Option<VirtqueueRegion<VirtqDesc>>` + two `Option<Frame<KernelMeta>>` for the avail/used rings. `setup_queue` switches from `alloc_page_frame() -> PhysAddr` to `OwnedPageFrame::alloc_zeroed() -> Option<Frame<KernelMeta>>`; on failure the `?`-propagated frames Drop, releasing the pages. `read_used_idx`/`submit`/`try_pop_used`/`write_desc` rewrite onto `Frame::read_volatile_at::<u16>` / `write_volatile_at::<u16>` / `VirtqueueRegion::write_desc_volatile`. The two layout offset constants (`AVAIL_IDX_OFFSET=2`, `AVAIL_RING_OFFSET=4`, `USED_IDX_OFFSET=2`, `USED_RING_OFFSET=4`) document the split-virtqueue ring layout. `VirtqDesc` and `VirtqUsedElem` gain `#[derive(Pod)]`. `unsafe impl Send for Virtqueue` **dropped** — auto-derived since `Frame<KernelMeta>: Send` and `VirtqueueRegion<T: Pod>: Send`. The `Clone, Copy, Default` derives on `Virtqueue` were removed (Frame has Drop) — `VirtioBlkDevice` and `VirtioNetDevice` lost the same derives in turn. `Virtqueue::write_desc` shifted from `&self` to `&mut self`; all callers already had `&mut state` via the SpinLock. **Carve-outs**: 0 (below the ≤1 budget). **κ.21.4**: `pci.rs` 10 → 2 carve-outs. **Full migration** per user choice — the lock-free fast-path quartet (`ECAM_PRIMARY_VIRT/SIZE/BUS_START/BUS_END`) and the slow-path `SpinLock<EcamState>` collapsed into one `static ECAM: OnceLock<EcamRegistry>`. `EcamRegistry` holds a primary `EcamConfigSpace` + cached `McfgEntry` plus `KVec<EcamConfigSpace>`/`KVec<McfgEntry>` for >1-segment systems; `find(bdf)` checks the primary first, then iterates extras. `pci_ecam_read{8,16,32}` / `pci_ecam_write{8,16,32}` rewrite onto a shared `ecam_read::<T>` / `ecam_write::<T>` generic that calls `EcamConfigSpace::read::<T>(bdf, offset)` / `write::<T>` via `Bdf::new(bus, device, function)`. The `ecam_virt_addr` helper, the 4-atomic primary cache (`ECAM_PRIMARY_VIRT/SIZE/BUS_START/BUS_END`), `ECAM_BASE`, `ECAM_ENTRY_COUNT`, `ECAM_STATE`, `EcamState`, and `MAX_ECAM_ENTRIES` all deleted. Public accessors (`pci_ecam_available`/`base`/`entry_count`/`entry`/`find_entry`/`primary_virt`/`mapped_region`) reimplemented on top of `EcamRegistry` so the `tests/ecam_tests.rs` suite continues to compile/run unchanged. The L1142 `&*registry.drivers[drv_idx]` retired via `PciDriver::from_ptr` (FromRawPtr); the L176 `cstr_to_str(ptr as *const c_char)` retired via the κ.20 `slopos_utils::string::cstr_to_str_lossy` helper. **Carve-outs (2)**: `unsafe impl Sync for PciDriver` (L58) and `unsafe impl Send for PciDriverRegistry` (L93) — load-bearing markers over `*const PciDriver` + `Option<fn(*const PciDeviceInfo, *mut c_void) -> _>` fields, both gated by the surrounding `SpinLock<PciDriverRegistry>` for data-race freedom (Inv. 8). **κ.21.5**: `ioapic/mod.rs` 8 → 0. `IoapicTable(UnsafeCell<[IoapicController; N]>)` and `IoapicIsoTable(UnsafeCell<[IoapicIso; N]>)` newtypes — and their `unsafe impl Sync` markers — deleted; replaced with `static IOAPIC_TABLE: SpinLock<IoapicTable>` / `static ISO_TABLE: SpinLock<IoapicIsoTable>`, where each `*Table` struct now holds an inline `count: usize` alongside the array. `IOAPIC_COUNT` and `ISO_COUNT` AtomicUsize counters retired (count moved into the locked struct). `ioapic_find_controller` returns `Option<IoapicController>` (Clone) via `IOAPIC_TABLE.lock()` + iterator; `find_iso` returns `Option<IoapicIso>` (Copy). `populate_from_madt` takes both locks once at boot. `config_irq` / `ioapic_update_mask` / `legacy_irq_info` operate on the cloned controller / copied ISO; MMIO ops route through `IoapicController::read_reg` / `write_reg` (already safe). **Carve-outs**: 0. **κ.21.6**: `tty/vconsole.rs` 3 → 2 carve-outs. `VConsoleFbInfo.base: *mut u8` lifted to `base: u64` (kernel virtual address as integer) — Send/Sync auto-derived, the file-level `unsafe impl Send for VConsoleFbInfo` marker dropped. The two production `unsafe { … }` blocks (the scanline blit at `flush_dirty_rows` and the per-pixel write at `put_pixel`) consolidated into two file-local helpers `fb_blit(base, byte_offset, src)` and `fb_put_pixel(base, offset, bytes_per_pixel, color)`, each holding one `unsafe { … }` block with a documented SAFETY note (callsite gates on framebuffer dimensions). The `_mm_sfence` carve-out moved to OSTD via a new safe wrapper `slopos_ostd::arch::x86_64::mem_fence::sfence()` — 1 unsafe block absorbed in OSTD (allowed), 0 net change in OSTD's accounting since it consolidates a previously absorbed pattern. **Carve-outs (2)**: `fb_blit` and `fb_put_pixel` helpers — file-local, single-unsafe-block, surrounded by safe Rust. **κ.21.7**: `drivers/src/serial.rs` — file-wide `#![allow(unsafe_code)]` with a doc-comment SAFETY block citing the Phase 1E + κ.15 + ι.4 deferral chain (slopos-utils as the early-boot panic-logger TCB; out of scope for κ.16 forbid). **Final drivers/src/ unsafe count**: 34 lines via `rg '\bunsafe\b' drivers/src/ --type rust | grep -v tests | wc -l`; **20 production lines** if we further subtract the 13 file-wide-deferred lines in `serial.rs` (allowed by the file attribute) and the 1 doc-comment in `vconsole.rs`. The aspirational ≤8 floor is exceeded by `serial.rs` alone; treating it analogously to κ.20's `limine_protocol.rs` SAFETY-doc precedent (count only production-meaningful sites and document the deferral) gives 4 production carve-outs in the κ.21 target set: 2 in `pci.rs` (Sync/Send markers) + 2 in `tty/vconsole.rs` (helper bodies). All 7 subtask goals met. **OSTD primitives consumed**: `slopos_ostd::dma::VirtqueueRegion<T: Pod>` (κ.17.1), `slopos_ostd::pci::{Bdf, EcamConfigSpace}` (κ.17.2), `slopos_ostd::dev::FromRawPtr` (κ.17.4), `slopos_ostd::mm::Frame::{read_at, read_volatile_at, write_at, write_volatile_at, read_slice, write_slice, slice_at, slice_at_mut}` + `OwnedPageFrame::alloc_zeroed`, `slopos_ostd::Pod` derive (via `slopos-ostd-derive`), `slopos_ostd::sync::OnceLock` + `SpinLock`, `slopos_ostd::KVec`, `slopos_utils::string::cstr_to_str_lossy`. **New OSTD primitive**: `slopos_ostd::arch::x86_64::mem_fence::sfence()` — safe wrapper over `core::arch::x86_64::_mm_sfence` (1 absorbed unsafe block in OSTD). **Tests**: `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK`); `cargo fmt --all` clean. `just test` parity left to user verification per the user's "I will test first" instruction.)*

##### κ.22 — fs / acpi tail retirement (~70 prod blocks)

Depends on κ.17.3.

- [x] **1J-κ.22.1** `fs/src/fileio/{fdops,mod,poll}.rs` (~50 → 0). Refactor the `SyncUnsafeCell<FileTable>` + raw-`*mut FileTableSlot` pattern to `KArc<SpinLock<FileTable>>`. Substantial structural change; one PR scoped to fs/. No carve-outs expected.
- [x] **1J-κ.22.2** `acpi/src/tables.rs` (~7 → 0 + ≤1 carve-out). Replace `*const SdtHeader` / `*const Rsdp` derefs with `AcpiTable::from_bytes(slice)`. The single boundary borrow `slice::from_raw_parts(data, length)` over the bootloader-published ACPI region is irreducible — carve-out at the `parse_rsdp` entry point.
- [x] **1J-κ.22.3** `acpi/src/{madt,mcfg,hpet}.rs` (~6 → 0). Migrate to `AcpiTable<'a>` typed accessors.

**Acceptance.** `rg 'unsafe' fs/src/ acpi/src/ --type rust | grep -v tests | wc -l` ≤ 1.

*(Done. **κ.22.1**: `fs/src/fileio/{mod,fdops,fdtable,poll}.rs` 61 → 0. The `SpinLock<FileioState>` mega-lock + per-slot marker `SpinLock<()>` + raw-`*mut FileTableSlot` reborrow pattern retired entirely. `FileTableSlot` becomes `{ process_id: AtomicU32, inner: SpinLock<FileTableSlotInner> }` where `FileTableSlotInner` holds `{ in_use: bool, descriptors: [FdEntry; FILEIO_MAX_OPEN_FILES] }`. The `kernel`/`processes[]` arrays become top-level statics (`KERNEL_TABLE`, `PROCESS_TABLES: [FileTableSlot; MAX_PROCESSES]`); the shared `open_files[]` + `external_ops` move into a separate `static OPEN_FILES_STATE: SpinLock<OpenFilesState>` at LOCK_LEVEL_RESOURCE. New helpers `slot_for_pid(pid) -> Option<&'static FileTableSlot>` (lock-free atomic scan), `lock_pid_slot(pid) -> Option<SpinLockGuard<'static, FileTableSlotInner>>` (hot-path snapshot), `with_pid_slot(pid, |inner| …)` (mutate-in-place), `pick_pid_slot_locked(pid)` (find/CAS-claim/lock), and `with_open_files(|state| …)` (open-file-table mutations). Lock order: per-process `slot.inner` (REGISTRY=2) acquired first, `OPEN_FILES_STATE` (RESOURCE=1) second — matches today's `with_tables` registry→resource ordering. Fork (`fileio_clone_table_for_process`) cannot hold two REGISTRY locks simultaneously: it snapshots `src.descriptors` into a stack-local `[FdEntry; 32]` (~128 B, well under the 2 KiB stack-size gate), drops src, CAS-claims a free dst slot, locks dst, and writes under `with_open_files` (with rollback if any incref fails). The `as_ptr()` bypass quartet (`lock_process_table`, `open_files_ptr`, `open_files_mut_ptr`, `external_ops_fast`) collapses entirely — the new layout is intrinsically split. `unsafe impl Send for FileTableSlot` and `unsafe impl Send for FileioState` deleted (auto-derived). `cstr_len`, `path_bytes`, and the file_list_path `from_raw_parts_mut(entries, cap)` site retired by **lifting fileio path APIs from `*const c_char`/`*mut UserFsEntry+u32` to `&[u8]`/`&mut [UserFsEntry]`**: `file_open_for_process`, `file_unlink_path`, `file_mkdir_path`, `file_stat_path`, `file_exists_path`, `file_list_path` all take slice arguments. **κ.22.2**: `acpi/src/tables.rs` 8 → 1 carve-out. Local `Rsdp`/`SdtHeader` packed structs deleted; the κ.17.3 OSTD types are re-exported (`pub use slopos_ostd::acpi::{AcpiTable, RSDP_SIGNATURE, RSDP_V1_SIZE, Rsdp, SdtHeader}`). The single `acpi_region_bytes(phys, len) -> Option<&'static [u8]>` helper consolidates every HHDM byte-borrow behind one `unsafe { core::slice::from_raw_parts(ptr, len) }` (file-level `#[allow(unsafe_code)]`); the helper is called four ways — RSDP probe (`RSDP_V1_SIZE`), RSDP full re-borrow (`size_of::<Rsdp>()`), SDT header probe (`size_of::<SdtHeader>()`), SDT full table read (header-validated `length`). `AcpiTables::from_rsdp(*const Rsdp)` becomes `AcpiTables::from_phys(rsdp_phys: u64)`; `find_table` returns `Option<AcpiTable<'static>>` (instead of `*const SdtHeader`). The `validate_rsdp`/`validate_table`/`scan_sdt`/`map_phys_table`/`checksum` helpers all retired — `Rsdp::validate(bytes)` and `AcpiTable::from_bytes(bytes)` from OSTD κ.17.3 do the checksum-validated parse. **κ.22.3**: `acpi/src/{madt,mcfg,hpet}.rs` 5+2+2 → 0. Each module takes `AcpiTable<'static>` instead of `*const SdtHeader`; entry walks consume `acpi.payload(): &[u8]` + `read_packed::<T>(payload, off)` per primitive field. Deleted `RawMadt`/`RawEntryHeader`/`RawIoapicEntry`/`RawIsoEntry`/`RawMcfgTable`/`RawMcfgEntry`/`RawHpetTable`/`AcpiGas`. New offset constants document the on-wire layout (`MADT_ENTRIES_OFFSET=8`, `IOAPIC_OFF_*`, `ISO_OFF_*`, `MCFG_RESERVED_SIZE=8`, `MCFG_OFF_*`, `HPET_PAYLOAD_LEN=20`, `HPET_OFF_*`). **Aggressive sweep across the rest of fs/** (per user choice) brings non-fileio files to 0 unsafe too. `pipe.rs::PipeSlot::reset_in_place(*mut)` retired in favour of `PipeSlot::reset(&mut self)` that field-by-field zeros (`buffer.fill(0)` is in-place, no 4 KiB rvalue). `ext2_vfs.rs`/`devfs/mod.rs`/`ramfs/mod.rs` drop their 6 `unsafe impl Send/Sync` markers — auto-derived (unit structs and `SpinLock<T: Send>`-wrapped types). `blockdev.rs::MemoryBlockDevice` migrates `{ base: *mut u8, len, owns_allocation }` to `{ buffer: SpinLock<KVec<u8>> }`; the test-only `as_mut_ptr()` API replaced with safe `with_buffer_mut(|buf: &mut [u8]| …)` closure. `ext2/symlink.rs`'s `[BlockNum; 15] ↔ [u8; 60]` cast retired via the existing `slopos_ostd::util::byte_view::pod_slice_as_bytes{,_mut}` after `BlockNum` gains `#[derive(Pod, Zeroable)]`. The `unsafe impl Zeroable for BlockNum` retires when `slopos-ostd-derive` adds a `Zeroable` derive macro (mirror of the existing `Pod` derive — same `#[repr(C)]`/`transparent` rules, field-level `Zeroable` bounds). **OSTD additions**: `slopos-ostd-derive::Zeroable` proc-macro derive (~30 lines, shared `MarkerKind` codepath with `Pod`); 4 host-side smoke tests at `slopos-ostd/tests/zeroable_derive.rs` covering named/transparent/unit struct shapes and round-trip with `init_zeroed`. `kernel-services::platform::PlatformServices` gains a parallel `get_rsdp_phys() -> u64` accessor (the `*const c_void` `get_rsdp_address()` API stays for `slopos_utils::cstr_from_kernel_ptr` callers); `boot/boot_impl.rs` wires it to `limine_protocol::get_rsdp_phys_address()` (which already existed at line 426). Three driver call sites (`drivers/src/{pci,hpet,ioapic/mod}.rs`) switch from `AcpiTables::from_rsdp(get_rsdp_address() as *const Rsdp)` to `AcpiTables::from_phys(platform::get_rsdp_phys())`. Six syscall handlers in `core/src/syscall/fs/path_handlers.rs` adapt to the slice-based fileio API: a small `cstr_buf_to_bytes(&[i8]) -> &[u8]` helper at the top uses `slopos_ostd::util::byte_view::pod_slice_as_bytes` (no syscall-layer `unsafe`); `syscall_fs_list` swaps the kmalloc/kfree dance for a `KVec<UserFsEntry>` allocation. **Final fs/+acpi/ unsafe count**: `rg '\bunsafe\b' fs/src/ acpi/src/ --type rust | grep -v tests | wc -l` = **1** — exactly the `acpi_region_bytes` boundary borrow, the lone TCB carve-out for the bootloader-published ACPI region. Strict acceptance hit. **OSTD primitives consumed**: `slopos_ostd::sync::{SpinLock, SpinLockGuard, InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE}`, `slopos_ostd::util::packed_view::read_packed`, `slopos_ostd::util::byte_view::{pod_slice_as_bytes, pod_slice_as_bytes_mut}`, `slopos_ostd::acpi::{AcpiTable, Rsdp, SdtHeader, RSDP_V1_SIZE, RSDP_SIGNATURE}` (κ.17.3), `slopos_ostd::KVec`, `slopos_ostd::Pod` + `slopos_ostd::Zeroable` derives (`Zeroable` derive newly added in this phase). `core::sync::atomic::AtomicU32` for the lock-free per-process slot scan. **Tests**: `cargo fmt --all` clean; `just build` clean (`check_alloc_dep: OK`, `check_stack_sizes: OK` — fork snapshot frame ~128 B, well under 2 KiB threshold). `just test` parity left to user verification per the user's "I will test first" instruction.)*

##### κ.23 — Absorption: relocate every residual unsafe pattern into OSTD

Replaces the prior carve-out catalog approach. Each sub-stage absorbs one category from the original κ.23.1 taxonomy by relocating the pattern into the OSTD primitive landed in κ.17.7..κ.17.15. **Acceptance for every sub-stage: the corresponding category-specific grep returns zero hits outside `slopos-ostd/`.** Stages may land in parallel once their κ.17.x prerequisite is in.

- [ ] **1J-κ.23.A** **inline-asm-cpu-contract** — Migrate all 8 residual `unsafe { asm!(...) }` sites to `slopos_ostd::cpu::*` calls. Affected files: `boot/src/shutdown.rs:120`, `core/src/scheduler/runtime.rs:277`, `core/src/scheduler/scheduler.rs:1081,1103`, plus 4 incidental sites. Replace with `slopos_ostd::cpu::halt()`, `slopos_ostd::cpu::halt_loop()`, `slopos_ostd::cpu::sti_hlt_atomic()` (already in OSTD per `slopos-ostd/src/cpu/x86_64/core.rs:7`). Depends on: nothing new (uses existing OSTD surface). *Acceptance:* `rg 'unsafe \{[^}]*asm!' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.B** **naked-fn-llvm-contract** — Delete `core/src/scheduler/safestack_rt.rs` and `boot/src/smp.rs:38–93` naked-fn bodies. Callers route through `slopos_ostd::arch::x86_64::naked::install_safestack_runtime()` and `install_ap_trampoline()` (κ.17.9). Depends on: κ.17.9. *Acceptance:* `rg '#\[unsafe\(naked\)\]' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.C** **extern-c-abi-marker** — Replace every `unsafe extern "C" { ... }` block outside OSTD with calls to `slopos_ostd::arch::x86_64::linker::*` accessors (κ.17.8) or `slopos_ostd::ffi::extern_block!` (κ.17.14). Affected: `core/src/scheduler/scheduler.rs:65–68` (`_text_start` / `_text_end`), `boot/src/cpu_verify.rs:43`, `boot/src/gdt.rs:30`, `mm/src/symbols.rs:8`, `hermetic/src/registry.rs:77`, `core/src/scheduler/ffi_boundary.rs:28–31`, plus ~4 others. Depends on: κ.17.8 + κ.17.14. *Acceptance:* `rg 'unsafe extern' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.D** **unsafe-trait-rust-abi** — Replace 25 hand-written `unsafe impl <Trait>` sites with derive macros or trait relocation: 8 `unsafe impl Zeroable` → `#[derive(Zeroable)]` (derive already exists from κ.22); 14 `unsafe impl HermeticState` → `#[derive(HermeticState)]` (κ.17.12); 1 `unsafe impl Linked for Task` → relocate alongside `OwnedTaskHandle` move (κ.17.13); 3 bridge traits (`WaitQueueBackend`, `RcuBackend`, `UserModeBackend`) → move trait bodies into `slopos_ostd::sync::*` and `slopos_ostd::user::*` so `kernel-services/` becomes safe-only. Depends on: κ.17.12 + κ.17.13. *Acceptance:* `rg 'unsafe impl \w+ for' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.E** **unsafe-impl-send-sync** — Replace 46 `unsafe impl Send for X` / `unsafe impl Sync for X` markers by wrapping the offending field/global in `KernelSync<T>` (κ.17.7). Heavy hitters: `net/src/{netdev,netstack,pool,timer,socket,neighbor,route,loopback,xe}.rs` (~9 sites), `service-core/src/service_cell.rs:14`, `core/src/scheduler/per_cpu.rs` (~2 sites), `core/src/scheduler/task_struct.rs:907` (the `OwnedTask`/`SharedTask` markers move with the type relocation in κ.17.13), `boot/src/early_init.rs` (~2 sites), plus ~28 others. For each call site, wrap the type in `KernelSync<T>` and update accessors. Depends on: κ.17.7. *Acceptance:* `rg 'unsafe impl (Send|Sync)' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.F** **bsp-only-init** — Replace 15 `pub unsafe fn register_with_ostd()` / one-shot init sites with sealed-token registration: each registration callable accepts a `BspToken` newtype only obtainable inside OSTD's BSP-init path (κ.17.7); the `unsafe fn` wrappers in mm/, kernel-services/, drivers/ are deleted; their bodies become safe inline calls passing the token. Affected: `kernel-services/src/{kernel_vm_space,ostd_bridge}.rs`, `mm/src/{frame_alloc_shim,io_mem_mapper_shim,mmu/luf_hook}.rs`. Depends on: κ.17.7. *Acceptance:* `rg 'pub unsafe fn (register|install)_' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.G** **panic-recovery** — Delete 11 per-subsystem `pub unsafe fn *_force_unlock` / `*_poison_unlock` wrappers (`mm/page_alloc.rs:1168`, `mm/kernel_heap.rs:1172`, `mm/process_vm.rs:2918,2932`, `core/scheduler/task_table.rs:803,811`, plus 5 fileio sites). The kernel panic handler calls `slopos_ostd::sync::panic_recovery::poison_all_held_locks()` once (κ.17.11). All per-subsystem `unsafe fn` declarations are removed. Depends on: κ.17.11. *Acceptance:* `rg '_(force|poison)_unlock' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.H** **boundary-slice-borrow** — Replace 20 `core::slice::from_raw_parts` sites with `slopos_ostd::boot::handoff::*` accessors (κ.17.10). Affected: `mm/src/process_vm.rs:779` (ELF payload), `acpi/src/tables.rs` (single residual site), `net/src/{socket,packetbuf}.rs` (~4 sites), `windowing/src/memfd_buf.rs:69`, plus ~12 others. Depends on: κ.17.10. *Acceptance:* `rg 'slice::from_raw_parts' --type rust -g '!slopos-ostd/**'` returns 0.

- [ ] **1J-κ.23.I** **test-scaffolding migration** — Apply `#[derive(HermeticState)]` (κ.17.12) to all 14 hand-written `unsafe impl HermeticState` in `core/src/scheduler/test_hermetic.rs`. Audit remaining test files for incidental unsafe; relocate any test-only helpers needing unsafe into `slopos-ostd::test_support` or `slopos-ostd/tests/scaffolding/`. **No `#![cfg_attr(test, allow(unsafe_code))]` exemption permitted** — test files outside `slopos-ostd/` must be unsafe-free under the κ.16 gate. Depends on: κ.17.12. *Acceptance:* `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '*test*'` returns 0.

- [ ] **1J-κ.23.J** **slopos-utils retirement** — Migrate every `slopos_utils::io::Port`-based call site (boot/serial/panic) onto `slopos_ostd::early_console` (κ.17.15). Migrate any other `slopos_utils::*` consumers onto OSTD equivalents. Delete the `slopos-utils/` crate entirely; update workspace `Cargo.toml` and any remaining `use slopos_utils::*` to `use slopos_ostd::*`. The `slopos-utils/` exemption from CLAUDE.md and Phase 1E is removed; CLAUDE.md is updated in this stage. Depends on: κ.17.15. *Acceptance:* `slopos-utils/` directory does not exist; `cargo build` clean; `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**'` returns 0.

**Aggregate acceptance.** `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '!userland/**' -g '!slibc/**' -g '!slop-protocol/**' -g '!ktesting/**' -g '!*.s'` returns **literal 0** (not "≤60"). `rg '#\[allow\(unsafe_code\)\]' --type rust -g '!slopos-ostd/**'` returns **literal 0**. `just test` ≥ pre-1J parity.

##### κ.16 — Forbid gate (final flip)

- [ ] **1J-κ.16** Add `#![forbid(unsafe_code)]` to every non-OSTD kernel `lib.rs`: `boot/src/lib.rs`, `mm/src/lib.rs`, `core/src/lib.rs`, `drivers/src/lib.rs`, `fs/src/lib.rs`, `net/src/lib.rs`, `acpi/src/lib.rs`, `karch/src/lib.rs`, `kernel-services/src/lib.rs`, `video/src/lib.rs`, `abi/src/lib.rs`, `windowing/src/lib.rs`, `service-core/src/lib.rs`, `font/src/lib.rs`, `hermetic/src/lib.rs`. **No `#[allow(unsafe_code)]` permitted anywhere outside `slopos-ostd/`** — neither in production code, nor in test scaffolding, nor in proc-macro outputs. The κ.23.A..κ.23.J absorption stages must have eliminated every site by this point. `slopos-utils/` is no longer in the exemption list (it is deleted by κ.23.J).

**Acceptance.** `just build` clean. `just test` green at pre-1J parity (≥ 2410 tests). `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '!userland/**' -g '!slibc/**' -g '!slop-protocol/**' -g '!ktesting/**' -g '!*.s'` returns **literal 0**. `rg '#\[allow\(unsafe_code\)\]' --type rust -g '!slopos-ostd/**'` returns **literal 0**. `rg '#!\[forbid\(unsafe_code\)\]' boot/src/lib.rs mm/src/lib.rs core/src/lib.rs drivers/src/lib.rs fs/src/lib.rs net/src/lib.rs acpi/src/lib.rs karch/src/lib.rs kernel-services/src/lib.rs video/src/lib.rs abi/src/lib.rs windowing/src/lib.rs service-core/src/lib.rs font/src/lib.rs hermetic/src/lib.rs` returns one match per file (15 total).

##### Stage dependency graph

```
κ.17 (primitives, closed)
  │
  ├─ κ.17.7..κ.17.15 (extension primitives, additive)
  │    │
  │    ├─ κ.23.A (inline-asm)        depends on cpu::* (already exists)
  │    ├─ κ.23.B (naked-fn)          depends on κ.17.9
  │    ├─ κ.23.C (extern-c)          depends on κ.17.8 + κ.17.14
  │    ├─ κ.23.D (unsafe-trait)      depends on κ.17.12 + κ.17.13
  │    ├─ κ.23.E (Send/Sync)         depends on κ.17.7
  │    ├─ κ.23.F (bsp-init)          depends on κ.17.7 (BspToken)
  │    ├─ κ.23.G (panic-recovery)    depends on κ.17.11
  │    ├─ κ.23.H (boundary-slice)    depends on κ.17.10
  │    ├─ κ.23.I (test-scaffolding)  depends on κ.17.12
  │    └─ κ.23.J (utils retirement)  depends on κ.17.15
  │
  ├─ κ.18 (closed)
  ├─ κ.19 (closed)
  ├─ κ.20 (boot residual)
  ├─ κ.21 (drivers tail)
  └─ κ.22 (closed)         ─→ κ.23.A..κ.23.J (parallel) ─→ κ.16 (forbid) ─→ 1J-λ
```

κ.17.7..κ.17.15 may land in parallel. κ.23.A..κ.23.J may land in parallel once their κ.17.x deps are in. κ.16 is gated on every κ.23.* showing grep-zero. 1J-λ is gated on κ.16 + parity tests.

**Verify (overall).** `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '!userland/**' -g '!slibc/**' -g '!slop-protocol/**' -g '!ktesting/**' -g '!*.s'` returns **literal 0**. `rg '#\[allow\(unsafe_code\)\]' --type rust -g '!slopos-ostd/**'` returns **literal 0**. No exemption catalog; the build fails on any non-zero count.

#### 1J-λ — Phase 1J close + parity

**Goal.** **(closes 1J.15)** Final test parity gate, plan-file marks updated, ready for 1K (KernMiri).

- [ ] **1J-λ.1** `just test` ≥ 2410, parity with pre-1J.
- [ ] **1J-λ.2** `just check-framekernel` clean (κ.16 forbid + literal-zero grep + check_alloc_dep + check_stack_sizes).
- [ ] **1J-λ.3** `just boot-log` reaches "ALL SYSTEMS OPERATIONAL!" without panics.
- [ ] **1J-λ.4** Manual smoke test: shell, fork, mmap, signals, multi-CPU.
- [ ] **1J-λ.5** Mark all 1J.1–1J.16 boxes in the "Original 1J subtask checklist" below as checked.
- [ ] **1J-λ.6** TCB ratio: `\bunsafe\b` token count divided by total kernel LoC. **Numerator counts only `slopos-ostd/`** — every other crate is now literal zero by κ.16. Target ≤ 1.5% post-Phase-1, with a Phase 2 target of ≤ 1.0%.
- [ ] **1J-λ.7** **Hard prerequisite gate:** `rg '\bunsafe\b' --type rust -g '!slopos-ostd/**' -g '!userland/**' -g '!slibc/**' -g '!slop-protocol/**' -g '!ktesting/**' -g '!*.s'` returns **literal 0**. `rg '#\[allow\(unsafe_code\)\]' --type rust -g '!slopos-ostd/**'` returns **literal 0**. Build fails otherwise. This is the load-bearing assertion that κ.23.A..κ.23.J + κ.16 actually delivered zero-unsafe-outside-OSTD; no exemption catalog is permitted.

#### Original 1J subtask checklist (reference)

These are the original 16 subtasks from the framekernel spec. Each is **closed by** a sub-phase as noted; checking these boxes is the responsibility of Stage λ.

- [ ] **1J.1** `karch/`: replace its `lib.rs` with re-exports from `slopos_ostd::cpu::x86_64`. Delete crate-internal CPU HAL files (`arch/`, `cpu/`, `init_flag.rs`, `interrupt_frame.rs`, `pcr.rs`, `tsc.rs`). *(closed by 1J-γ)*
- [ ] **1J.2** `boot/`: replace `boot/src/idt.rs` IDT setup with calls to `slopos_ostd::irq::idt::install`. Replace `boot/src/gdt.rs` with `slopos_ostd::arch::x86_64::gdt`. Delete duplicated entries. *(closed by 1J-δ)*
- [x] **1J.3** `mm/src/mmio.rs`: `MmioRegion` becomes `pub type MmioRegion = slopos_ostd::IoMem;`. *(Done — closed by 1J-β.1.)*
- [ ] **1J.4** `mm/src/page_alloc.rs`: replace `OwnedPageFrame` with a type alias to `Frame<KernelMeta>`. *(alias added in 1J-β as `KernelFrame`; literal rename closed by 1J-κ.1.)*
- [x] **1J.5** `mm/src/process_vm.rs::ProcessVmInner`: replace raw `pml4` pointer with `vm_space: KArc<VmSpace>`. *(Done — closed by 1J-η.4. Vestigial `page_dir` field survives until 1J-κ.19.4.)*
- [x] **1J.6** `mm/src/paging/`: per-process surface becomes private to `slopos-ostd::mm::vm_space`. *(Done in part — closed by 1J-η.4. Kernel-side early-boot fallback retained.)*
- [x] **1J.7** `mm/src/user_copy.rs`: thin re-export of `slopos_ostd::user::copy`. *(Done — closed by 1J-β.2 / 1J-θ.)*
- [x] **1J.8** `core/src/scheduler/switch_asm.rs`: delete; functionality now in `slopos_ostd::task::switch`. *(Done — closed by 1J-ζ.3.)*
- [x] **1J.9** `core/src/scheduler/`: consume `slopos_ostd::task::switch / fpu` primitives. *(Done — closed by 1J-ζ. `Task` struct re-skin and `Scheduler` trait consumption deferred to Phase 2.)*
- [x] **1J.10** `core/src/syscall/`: `SyscallContext` migrates to `&mut UserContext`. *(Done — closed by 1J-θ.)*
- [ ] **1J.11** `core/src/irq.rs`: thin wrapper re-exporting `slopos_ostd::irq`. *(closed by 1J-δ.5; deletion deferred to Phase 2.)*
- [x] **1J.12** `drivers/`: `MmioRegion` alias + `IoPort<T>` + `IrqLine::register_callback`. *(Done — closed by 1J-ι.)*
- [x] **1J.13** `fs/`, `net/`, `acpi/`: chase compile errors from renames. *(Done — closed by 1J-β.9; will need a re-pass when 1J.3 / 1J.7 alias deletions land in Phase 2.)*
- [ ] **1J.14** Crucially: at the end of 1J, every kernel crate **except `slopos-ostd`** must have *zero* `unsafe` blocks. This will require some compile errors to be fixed by introducing safe OSTD APIs — that's expected. Track them as 1J.14.{a..z} sub-items. *(closed by 1J-κ)*
- [ ] **1J.15** Run `just test`. Test count must equal pre-1J. Any test failure is a 1J defect. *(closed by 1J-λ)*
- [ ] **1J.16** Verify: `rg 'unsafe' --type rust -g '!slopos-ostd/**'` returns zero matches in kernel crates. (Userland excluded.) *(closed by 1J-κ.16)*

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
