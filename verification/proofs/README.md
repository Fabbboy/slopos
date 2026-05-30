# Verus proofs

Each `*.rs` file directly in this directory is a **standalone Verus
crate-of-one** that mirrors a slice of `slopos-ostd` under `verus! { ... }`
and states its load-bearing invariants with `requires` / `ensures` /
`invariant`. `just verify` (`scripts/verify.sh`) runs the pinned Verus
toolchain over each one and fails on any unverified obligation.

## Conventions

- One proof file per OSTD critical-path module. Name it after the module:
  `frame_refcount.rs`, `slab_lifetime.rs`, `vm_space_cursor.rs`.
- A file whose name starts with `_` is a **shared helper module** — it is
  `include!`d by the proofs that need it and is **not** run as a top-level
  entry point by `verify.sh`.
- Proofs `use vstd::prelude::*;` and therefore only build under the Verus
  toolchain, never under the kernel's nightly. They are not cargo targets.
- When a proof closes, update `../STATUS.md` (mark the module **verified**
  against the pinned Verus SHA) and port the Verus-annotated source back
  into `slopos-ostd` (Verus emits a normal build for the kernel) so
  `just build` + `just test` still pass.

`frame_refcount.rs` (Phase 3B) is the first proof to land: 9 obligations
machine-checking the `Frame<M>` reference-count invariants (I1 allocated-
while-referenced, I2 release-exactly-once, I3 no clone/drop use-after-free)
plus a load-bearing witness that the broken `fetch_add(1)` clone violates
them.

`slab_lifetime.rs` (Phase 3C) is the second: 11 obligations machine-checking
the slab object lifecycle — Inv. 9 (a slot cannot outlive its parent slab:
an outstanding cell pins its page) and Inv. 10 (a slot is only used for an
object that fits its size + alignment) — plus load-bearing witnesses that an
unconditional page reclaim breaks Inv. 9 and an always-smallest size class
breaks Inv. 10. See `../STATUS.md` for the per-obligation summaries.
