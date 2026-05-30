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
| `slopos_ostd::mm::heap` (`Slab` / `HeapSlot`) | `proofs/slab_lifetime.rs` | Inv. 9 (slot ⊄ outlives slab), Inv. 10 (size/align fit) | **planned** |
| `slopos_ostd::mm::vm_space` (`Cursor`) | `proofs/vm_space_cursor.rs` | PT well-formedness; range-disjoint cursors; Inv. 4 + Inv. 5 across map/unmap | **planned** |

> `proofs/frame_refcount.rs` (Phase 3B) is **verified** — 9 obligations
> check on the pinned Verus SHA via `just verify`. The remaining two rows
> become **verified** as their proof files land (Phase 3C / 3D). `just
> verify` now machine-checks the frame proof on every run.

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

## Everything else

All other `slopos-ostd` modules are **audited only** — covered by the
KernMiri suite (`just check-miri`) and the `// SAFETY:` invariant
annotations (every `unsafe` block names at least one of Inv. 1–10), but not
machine-checked. Full-OSTD Verus verification is out of scope;
critical-path verification gets the bulk of the credibility for a small
fraction of the cost.
