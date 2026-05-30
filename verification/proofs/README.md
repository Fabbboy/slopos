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

No proofs exist yet — only the toolchain pin + harness are in place. The
first proof to land is `Frame<M>` reference counting.
