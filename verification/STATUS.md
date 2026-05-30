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
| `slopos_ostd::mm::frame` (`Frame<M>` ref-count) | `proofs/frame_refcount.rs` | Frame ref-count soundness; `Drop` releases once; no clone/drop UAF | **planned** |
| `slopos_ostd::mm::heap` (`Slab` / `HeapSlot`) | `proofs/slab_lifetime.rs` | Inv. 9 (slot ⊄ outlives slab), Inv. 10 (size/align fit) | **planned** |
| `slopos_ostd::mm::vm_space` (`Cursor`) | `proofs/vm_space_cursor.rs` | PT well-formedness; range-disjoint cursors; Inv. 4 + Inv. 5 across map/unmap | **planned** |

> No proofs are authored yet — only the toolchain pin and the `just verify`
> harness are in place. The three rows above become **verified** as each
> proof file lands. `just verify` is a green no-op until then.

## Everything else

All other `slopos-ostd` modules are **audited only** — covered by the
KernMiri suite (`just check-miri`) and the `// SAFETY:` invariant
annotations (every `unsafe` block names at least one of Inv. 1–10), but not
machine-checked. Full-OSTD Verus verification is out of scope;
critical-path verification gets the bulk of the credibility for a small
fraction of the cost.
