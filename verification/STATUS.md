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
| `slopos_ostd::mm::vm_space` (`Cursor`) | `proofs/vm_space_cursor.rs` | PT well-formedness; range-disjoint cursors; Inv. 4 + Inv. 5 across map/unmap | **planned** |

> `proofs/frame_refcount.rs` (Phase 3B, 9 obligations) and
> `proofs/slab_lifetime.rs` (Phase 3C, 11 obligations) are **verified** —
> 20 obligations check on the pinned Verus SHA via `just verify`. The last
> row becomes **verified** as `proofs/vm_space_cursor.rs` lands (Phase 3D).
> `just verify` machine-checks both proofs on every run.

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

## Everything else

All other `slopos-ostd` modules are **audited only** — covered by the
KernMiri suite (`just check-miri`) and the `// SAFETY:` invariant
annotations (every `unsafe` block names at least one of Inv. 1–10), but not
machine-checked. Full-OSTD Verus verification is out of scope;
critical-path verification gets the bulk of the credibility for a small
fraction of the cost.
