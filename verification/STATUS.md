# OSTD Verification Status

Per-module verification status for the OSTD critical path.

- **Pinned Verus**: `0.2026.05.24.ecee80a`
  (commit `ecee80a2139923d503338e6989f79fb690ec7847`) — see `verus.toml`.
- **Status legend**:
  - **verified** — a Verus proof in `proofs/` checks the module's
    load-bearing invariants on the pinned toolchain.
  - **audited only** — manually reviewed against the invariants, no
    machine-checked proof yet.
  - **unaudited** — no formal treatment yet.

## Critical-path modules (proof targets)

| OSTD module | Proof file | Invariants | Status |
|---|---|---|---|
| `slopos_ostd::mm::frame` (`Frame<M>` ref-count) | `proofs/frame_refcount.rs` | Frame ref-count soundness; `Drop` releases once; no clone/drop UAF | **verified** |
| `slopos_ostd::mm::slab` (`Slab` / `HeapSlot`) | `proofs/slab_lifetime.rs` | Inv. 9 (slot ⊄ outlives slab), Inv. 10 (size/align fit) | **verified** |
| `slopos_ostd::mm::vm_space` (`Cursor`) | `proofs/vm_space_cursor.rs` | PT well-formedness; range-disjoint cursors; Inv. 4 + Inv. 5 across map/unmap | **verified** |

> `proofs/frame_refcount.rs` (Phase 3B, 9 obligations),
> `proofs/slab_lifetime.rs` (Phase 3C, 11 obligations), and
> `proofs/vm_space_cursor.rs` (Phase 3D, 12 obligations) are all
> **verified** — 32 obligations check on the pinned Verus SHA via
> `just verify`. The OSTD critical path is **3/3 proofs**. `just verify`
> machine-checks every proof on every run.

### `slopos_ostd::mm::frame` — proof summary (Phase 3B)

`proofs/frame_refcount.rs` is a Verus-annotated mirror of the
atomic-bounded reference-count state machine in `frame.rs`
(`MetaSlot` / `Frame::from_unused` / `Frame::from_in_use` / `Drop`). It
models each atomic-bounded method body as one `Step` against an abstract
`Slot` and proves an inductive invariant (`slot_inv`) survives every step,
then lifts that to *every finite trace of steps* — i.e. every concurrent
interleaving of clone/drop calls — via `invariant_holds_on_every_trace`.
The three § 3B.2 obligations land as named corollaries:

- **(I1)** `i1_positive_rc_is_allocated` — `ref_count > 0` ⇒ the frame is
  typed (allocated) and off the allocator free list.
- **(I2)** `i2_release_at_most_once` + `i2_dropfinal_releases_once` —
  `Drop` releases the frame exactly once on the transition to 0; no other
  step touches the release counter, so a double-free is unreachable.
- **(I3)** `i3_no_use_after_free` — a live payload and free-list
  membership are mutually exclusive in every reachable state.

The proof is *load-bearing*, not vacuous: `broken_clone_violates_invariant`
encodes the unconditional `fetch_add(1)` clone (the Asterinas paper Fig. 9
UB) and proves it drives the invariant false, while the shipped conditional
`fetch_update` clone preserves it — so soundness genuinely depends on the
refuse-to-revive increment in `Frame::from_in_use`.

9 obligations, verified on Verus `0.2026.05.24.ecee80a`.

### `slopos_ostd::mm::slab` — proof summary (Phase 3C)

`proofs/slab_lifetime.rs` is a Verus-annotated mirror of the slab object
lifecycle: the kernel-side `mm::slab::allocator::SlabAllocator<SIZE>`
grow/alloc/dealloc critical sections and `mm::slab::KernelSlab`'s
size-class dispatch, behind OSTD's `mm::slab::Slab` trait. It splits into
two parts, one per invariant.

**Part 1 — Inv. 9 (a slot cannot outlive its slab).** Each slab critical
section is modelled as one `Step` against an abstract `SlabState` (live
page, capacity, free, outstanding). An inductive invariant (`slab_inv`)
survives every step and is lifted to *every finite trace of steps* — every
concurrent interleaving of grow/alloc/dealloc/reclaim — via
`invariant_holds_on_every_trace`. The § 3C.2 Inv. 9 obligation lands as:

- `inv9_outstanding_implies_live` — an outstanding cell pins its page
  (`outstanding > 0 ⇒ live`), so no slot points into buddy-reclaimed memory.
- `inv9_dead_slab_has_no_slots` — a reclaimed page has zero outstanding
  cells (the contrapositive).
- `inv9_no_reclaim_with_outstanding` — `Reclaim` is a no-op while a cell is
  outstanding (the step-level guard).

**Part 2 — Inv. 10 (size/align fit).** The size-class chooser (`class_size`,
mirroring `KernelSlab::class_of` after the 16-byte round-up) is proved to
return a cell at least as large as the request (`class_size_covers`), and
the slab's 16-byte cell alignment covers `align_of::<T>() <= 16`. The
§ 3C.2 Inv. 10 obligation lands as `inv10_into_box_fits`: for any in-range
`T`, the cell `KernelSlab::alloc` returns admits `into_box::<T>`.

Both halves are *load-bearing*, not vacuous:
`broken_reclaim_violates_invariant` encodes an unconditional page reclaim
(free a page with live cells) and proves it drives Inv. 9 false, while the
`outstanding == 0`-guarded reclaim preserves it; `undersized_class_violates_inv10`
encodes an always-smallest (16-byte) chooser and proves it lets a
2048-byte object overflow a 16-byte cell, while the real scan fits every
in-range request.

11 obligations, verified on Verus `0.2026.05.24.ecee80a`.

### `slopos_ostd::mm::vm_space` — proof summary (Phase 3D)

`proofs/vm_space_cursor.rs` is a Verus-annotated mirror of the page-table
mutation path in `vm_space.rs`: the `CursorMut::map` / `unmap` / `protect`
operations that walk the 4-level x86_64 page table
(`page_table.rs::walk_to_leaf`) and the `Frame<M>` ref-count leak/reclaim
discipline backing a leaf mapping. CortenMM (SOSP '25 Best Paper) is the
reference design for verified concurrent paging; `notes/cortenmm.md`
records the mapping — CortenMM's transactional `AddrSpace::lock(r) ->
RCursor` is exactly SlopOS's `VmSpace::cursor_mut(range) -> CursorMut`.

**Concurrency model — coarse lock-per-`VmSpace` (Phase 3D.3 fallback).**
Where CortenMM uses per-PT-page locks + an RCU monitor and proves
fine-grained mutual exclusion (its property P1), SlopOS uses the **Rust
borrow checker**: `CursorMut<'a>` holds `&'a mut VmSpace`, so at most one
mutator exists per address space at any instant, statically, with **no SMT
obligation**. This is the coarse model Phase 3D.3 explicitly sanctions
when the fine-grained one is not needed. It is strictly more conservative
than CortenMM's range-disjoint parallelism (it serializes even disjoint
ranges on one space), so it forbids strictly more concurrency: a
**scalability** gap, not a **soundness** one. CortenMM's hardest
obligation (P1, fine-grained mutual exclusion + RCU stale-retry) therefore
**does not arise** in SlopOS. What the proof discharges is CortenMM's P2 —
page-table well-formedness and the functional correctness of map/unmap
under the serialized op stream the exclusive borrow produces.

Each cursor critical section is modelled as one `Step` against an abstract
`PtPath` (the PML4 -> PDPT -> PD -> PT -> leaf chain plus the leaf PTE's
leaked-ref count). An inductive invariant (`pt_inv`) survives every step
and is lifted to every trace via `invariant_holds_on_every_trace`. The
§3D.2 obligations land as named corollaries:

- **(WF)** `wf_no_dangling_intermediate` — a present leaf implies its whole
  intermediate chain (PT, PD, PDPT) is present and valid, so no walk
  dereferences a dangling table. CortenMM Fig. 12 for the 4-level walk.
- **(REF)** `ref_leaf_holds_at_most_one` + `ref_map_unmap_exactly_once` +
  `ref_map_then_unmap_roundtrips` — the leaf PTE holds at most one leaked
  `UFrame` ref; `map` into an empty leaf leaks exactly one; `unmap` of a
  present leaf reclaims exactly one; the round trip returns to zero. No
  double-leak, no double-free.
- **(Inv. 4 + Inv. 5)** `inv45_leaf_is_uframe` — a present user leaf is
  always an insensitive `UFrame`; the `map::<S, M: AnyUFrameMeta>(UFrame<M>,
  ..)` argument type is the carrier.
- **(DIS)** `disjoint_vmspaces_independent` — two live cursors necessarily
  hold `&mut` to distinct `VmSpace`s, so their states are independent
  values and stepping one cannot mutate the other. The coarse-model
  discharge of CortenMM's §3.3 range-disjoint semantics.

Both guards are *load-bearing*, not vacuous:
`broken_double_leak_violates_refcount` proves removing the `Overlap` guard
lets `map` leak a second ref into a present leaf (stranding one on the next
`unmap`), and `broken_map_sensitive_violates_inv45` proves accepting a raw
`Frame` instead of a `UFrame` lets a sensitive frame land in a user PTE —
while the shipped Overlap-guarded, `UFrame`-typed `map` preserves the
invariant on every state. Source cross-referenced (`vm_space.rs`
`# Verification` module-doc; `VERIFIED:` notes on the `map` Overlap guard,
the `UFrame` leak, and the `unmap` reclaim).

**Gap recorded (Phase 3D.3 / R11).** SlopOS serializes all cursor
mutations on one `VmSpace`, including those over disjoint ranges, where
CortenMM would run them in parallel. The fine-grained, in-one-space
range-disjoint parallel proof would require per-PT-page locking SlopOS does
not have; re-attempt on each Verus bump (3A.4) if SlopOS ever grows it.
Until then the coarse model is the honest and sufficient one.

12 obligations, verified on Verus `0.2026.05.24.ecee80a`.

## Everything else

All other `slopos-ostd` modules are **audited only** — covered by the
KernMiri suite (`just check-miri`) and the `// SAFETY:` invariant
annotations (every `unsafe` block names at least one of Inv. 1–10), but not
machine-checked. Full-OSTD Verus verification is out of scope;
critical-path verification gets the bulk of the credibility for a small
fraction of the cost.
