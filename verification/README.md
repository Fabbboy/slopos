# SlopOS OSTD Verification

Machine-checked proofs of the OSTD critical-path invariants, on a pinned
[Verus](https://github.com/verus-lang/verus) toolchain. This directory is
the verification surface of the framekernel: it puts the load-bearing
`slopos-ostd` invariants under a static verifier while the trusted core is
still small enough to verify.

> **Why Verus, why pinned?** AD-10: Verus is the verifier (best-fit for
> systems Rust; Asterinas's choice via `vostd`). It is pinned to a single
> upstream **stable release on `main`** — never the experimental `async`
> branch, because OSTD is sync (AD-8 / AD-9) and the `async` branch does
> not support trait async, which OSTD's traits all are.

## Layout

| Path | Purpose |
|---|---|
| `verus.toml` | The pin: release tag, commit SHA, asset URL + sha256, and the Rust toolchain Verus links against. **Single source of truth.** |
| `proofs/` | Standalone Verus proof files. Each `*.rs` is a crate-of-one run by `just verify`. Files starting with `_` are shared helper modules (`include!`d, not run directly). |
| `notes/` | Background notes (e.g. `cortenmm.md` for the `VmSpace::cursor` proof). |
| `STATUS.md` | Per-OSTD-module verification status: **verified** / **audited only** / **unaudited**, with proof-file links and the pinned SHA. |
| `src/lib.rs` | Documentation-only crate target so `verification/` is a first-class workspace member. The proofs are *not* cargo targets. |
| `Cargo.toml` | Workspace-member manifest for the doc crate above. |

## Running the verifier

```sh
just ensure-verus      # download + pin Verus under third_party/verus
just verify            # check every proof in proofs/
just verify frame_refcount   # check a single proof by file stem
```

`just verify` resolves the pinned Verus via `scripts/ensure_verus.sh`
(which downloads the release asset named in `verus.toml`, verifies its
sha256, unpacks it under `third_party/verus/`, and installs the Rust
toolchain Verus links against). It then runs `scripts/verify.sh`, which
invokes `verus --crate-type=lib` over each proof and fails the gate on any
unverified obligation — exactly like the other framekernel-discipline
gates.

With **no proofs present** `just verify` is a green no-op, so the CI gate
can go live immediately, before the first proof is authored.

### Host requirements

The prebuilt Verus asset pinned in `verus.toml` is **x86_64 Linux only**
(AD-13 keeps us single-arch). `ensure_verus.sh` installs
the Rust toolchain Verus needs (`rustc-dev` + `llvm-tools` components) via
`rustup` on demand; it is independent of the kernel's nightly toolchain
(`rust-toolchain.toml` at the repo root).

On other hosts, build Verus from the pinned commit
(`verus.toml → [verus].commit`) following upstream's build instructions and
drop the resulting launcher at `third_party/verus/verus`, or point
`VERUS_BIN` at it: `VERUS_BIN=/path/to/verus just verify`.

## CI gate

Any PR that touches `slopos-ostd/` must pass `just verify`. Proof
regressions block merge. `just verify` is also part of the composite
`just check-framekernel` gate.

The sibling `scripts/check_no_kernel_async.sh` gate (also in
`check-framekernel`) enforces AD-8 / AD-9: no kernel crate — OSTD,
services, or the future io_uring-style `ring/` crate — may contain an
`async fn`. Async lives in userspace on top of the ring surface.

## Upgrading the pinned Verus toolchain

> **At most once per quarter**, and only when a needed feature lands.

1. Work on a **`verify/<sha>` topic branch** — never bump the pin on a
   feature branch.
2. Pick the new target: the latest **stable release on `main`**
   (`https://github.com/verus-lang/verus/releases`). **Do not** use the
   `async` branch.
3. Update `verus.toml`: `release`, `commit`, `version`, `rust_toolchain`,
   `[asset].url`, and `[asset].sha256` (the new asset's digest — release
   assets are immutable, so this is a stable integrity anchor). Mirror the
   `version` / `commit` constants in `src/lib.rs`.
4. `rm -rf third_party/verus` and re-run `just ensure-verus` to fetch the
   new build.
5. `just verify` must pass clean. If a proof breaks on the new Verus,
   either fix the proof on the topic branch or revert the bump — **never**
   weaken an invariant to make a bump go green.
6. Update `STATUS.md`'s "pinned Verus SHA" line and re-attempt any
   proof that previously fell back to a coarser model (e.g. the
   `VmSpace::cursor` fine-grained concurrent proof).
7. Land the topic branch only after CI is green.

### If Verus stops shipping

Fall back to [Kani](https://github.com/model-checking/kani) for bounded
model checking of the same invariants. Kani is a bounded checker,
not a full verifier, so document the reduced guarantee in `STATUS.md`.

## References

- Verus guide — https://verus-lang.github.io/verus/guide/
- vostd (Verus verification of Asterinas OSTD) — https://github.com/asterinas/vostd
- CortenMM (verified concurrent paging, prior art for the cursor proof) —
  http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf
