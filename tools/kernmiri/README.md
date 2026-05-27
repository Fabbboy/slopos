# KernMiri — dynamic UB detection on `slopos-ostd`

This directory anchors SlopOS's KernMiri harness — the Phase 1B deliverable
from `plans/FRAMEKERNEL_PLAN.md`. It's a thin layer of `cfg(target_os =
"none")` host stubs inside `slopos-ostd/` plus a `just check-miri` recipe;
there is no fork of Miri and no separate harness crate.

## What this is

Asterinas's [KernMiri](https://arxiv.org/abs/2506.03876) (USENIX ATC '25)
is roughly 1,200 lines of shims that let stock Miri interpret the OSTD
kernel core. We follow the same pattern: every hardware-touching primitive
in `slopos-ostd` has a `cfg(not(target_os = "none"))` fallback (or, for
the heaviest naked-asm sites, a `cfg(miri)` / `#[cfg_attr(miri, ignore)]`
escape hatch). The OSTD algorithms — `Frame<M>` ref counting, `VmSpace`
cursor walking, `Slab` / `HeapSlot` lifetimes, spinlock / RCU / wait-queue
protocols — execute under Miri unchanged. Miri then watches every memory
op those algorithms perform and reports UB at the instant it happens.

## What this catches

The 154 lib unit tests + ~330 integration tests cover, among others:

- **Stacked / Tree Borrows violations** — the bug class that's most likely
  to hide inside the `unsafe` blocks in `slopos-ostd`. Code review and
  `cargo test` cannot see them.
- **Provenance violations** — pointers used outside the allocation they
  were derived from.
- **Use-after-free / double-free / OOB** in the heap, slab, and frame
  allocators.
- **Data races** on non-atomic memory across spawned threads.
- **Uninitialized reads** — anywhere `MaybeUninit` is dereferenced
  prematurely.
- **Alignment violations** — exactly the bug Miri caught in
  `slopos-ostd/src/util/ptr_buf.rs::borrow_at_mut` during this phase.

## How to run

```
rustup component add miri --toolchain nightly-2026-05-25
cargo +nightly-2026-05-25 miri setup     # one-time, ~5–10 min (builds Miri's std)
just check-miri
```

`just check-miri` runs the full slopos-ostd unit + integration test
suite under Miri with these flags:

- `-Zmiri-disable-isolation` — allow real time access so RCU's
  `rdtsc`-backed clock makes progress.
- `-Zmiri-ignore-leaks` — host scratch allocations in
  `tests/{vm_space,uframe_round_trip,dma,io_mem,ecam,user_mode,virtqueue}.rs`
  are intentionally permanent (the test backing store outlives the
  OSTD references into it); their leakage is not a finding.

Miri runs in its **default provenance mode**, which permits the
`expose_provenance()` / `with_exposed_provenance[_mut]()` round-trip
that OSTD's u64-typed phys-to-virt model relies on (see "Why not
strict provenance?" below).

## Provenance discipline (and why not `-Zmiri-strict-provenance`)

The test setup files and a small set of OSTD primitives
(`mm::phys::phys_to_virt`, `mm::io_mem::IoMem::{read,write}_volatile`,
`boot::handoff::acpi`) use the explicit
`core::ptr::with_exposed_provenance[_mut]` API together with a
matching `.expose_provenance()` call on the backing allocation. That
makes the intent of every integer-to-pointer round-trip auditable
and lets Miri's default-mode provenance model narrow the alias set
the synthesized pointer can reach.

We do **not** run `-Zmiri-strict-provenance`. Strict mode forbids
`with_exposed_provenance` outright — under strict provenance the
only legal way to change a pointer's address is `ptr.with_addr(...)`,
which requires keeping a live source pointer alongside the address.
OSTD models hardware-derived virtual addresses as bare `u64` (the
HHDM offset, MMIO virt base, ACPI table base) and there is no live
source pointer in the real kernel for those values — strict
provenance would require restructuring `FrameAlloc`, `IoMemMapper`,
and the HHDM contract to carry a `*mut u8` alongside every u64,
which is a Phase 2-scale refactor. Sentinel-token tests in
`tests/lock_graph.rs` and `tests/panic_recovery.rs` already use
`core::ptr::without_provenance(...)` (the strict-provenance-clean
construction for "opaque integer that is never dereferenced").

## CI integration

`.github/workflows/ci.yml` runs `just check-miri` in a dedicated `miri`
job that executes in parallel with the existing `Build, Format & Test`
job. UB caught by Miri blocks merge to `develop` the same way a failed
kernel build or test failure does.

Three cache layers keep the CI job fast (~3 min on warm caches, ~10 min
on the first run after a Rust toolchain bump):

| Cache | Path | Key | Why |
|---|---|---|---|
| Miri sysroot | `~/.cache/miri/` | `hashFiles('rust-toolchain.toml')` | The expensive 5–10 min build; only re-runs when the pinned nightly changes. |
| Cargo registry + Miri target dir | `~/.cargo/...` + `builddir/target/` | `Swatinem/rust-cache@v2` with `prefix-key: v0-miri` | Separate from the main `ci` job's cache so they don't conflict on `target/.rustc_info.json`. |
| `miri` rustup component | toolchain dir | toolchain hash | `just setup` installs it via the `components` list in `rust-toolchain.toml`; rustup persists it. |

Run locally to mirror what CI does:

```
just check-miri
```

The same recipe runs in CI, so anything that's green locally is green in
CI (subject to runner-side cache misses extending the wall time).

## Integration model

| Layer | Mechanism | Use |
|---|---|---|
| Host-vs-kernel impl pivot | `cfg(target_os = "none")` vs `cfg(not(target_os = "none"))` | Body-level fallbacks for `read_cr3`, `wrmsr`, port I/O, `cli`/`sti`, `invlpg`, etc. Miri uses the host triple, so the not-none branch is automatically chosen. |
| Miri-only impl pivot | `cfg(miri)` (auto-set by cargo-miri) | A handful of test-support fns whose real impl is `unsafe { asm!(...) }` — `read_cs`, `sgdt`, `read_lsr`, etc. |
| Miri-only test skip | `#[cfg_attr(miri, ignore)]` | Tests that exercise the heaviest naked-asm sites (`tests/user_mode.rs`, `panic_recovery.rs`, parts of `task_handles.rs`), the `__ostd_usercopy_start/_end` binary layout, and a handful of `extern static` lookups that Miri does not model. |
| Dev wiring | `test-helpers` feature, auto-enabled by `dev-dependencies` | Exposes test-only constructors. `cargo miri test` picks it up automatically; no `--features` flag needed. |

We do **not** fork Miri. We do **not** ship a separate harness crate. The
shim layer lives entirely inside `slopos-ostd/`, follows the existing
`cfg(target_os = "none")` discipline already used by
`slopos-ostd/src/early_console.rs`, and stays out of the way of the real
kernel build.

## What runs vs. what's ignored

| Test target | Run under Miri | Ignored under Miri |
|---|---|---|
| `--lib` (in-tree `#[cfg(test)]`) | 154 | 2 (`user::copy::fault_range_*`) |
| `tests/extern_block.rs` | 1 | 5 (`unsafe extern static` resolution) |
| `tests/kernel_sync.rs` | 13 | 1 (`RefCell::borrow()` counter race) |
| `tests/user_mode.rs` | 0 | 20 (naked-asm user-mode entry path) |
| Doctest binary | 10 (`compile_fail` doctests) | 20 (` ```ignore ` doctests) |
| Everything else | all | 0 |

Total: **395 pass, 28 integration-test-side ignored, 20 doctest-side
ignored**.

### About the 20 ignored doctests

These are not bugs and not test gaps — they're documentation snippets
fenced with ` ```ignore `. Each one shows the *syntax* of a macro or
API usage but references symbols that don't exist at doctest scope
(placeholder type names like `MyHandle` / `MyState`, kernel-only
runtime context like `#[global_allocator]` / boot-init, or macro
invocations that need surrounding scaffolding). They are:

| Location | Kind |
|---|---|
| `arch/x86_64/cpuid.rs::XsaveFeatures` | "during boot" usage pattern |
| `cpu/x86_64/control_regs.rs::stac` | tight `stac()` / user-page touch / `clac()` window |
| `dev/mod.rs::FromRawPtr` | trait usage referencing user's `MyHandle` |
| `ffi/mod.rs` (5×) | `extern_block!` / `link_section_static!` / `extern_c_entry!` macro syntax |
| `klog.rs` | driver-side `klog_register_backend` registration |
| `mm/heap.rs::KernelHeap` | `#[global_allocator]` site (kernel `main.rs` is the only consumer) |
| `mm/init.rs` (6×) | `write_field!` / `write_array_field!` / `write_init_field!` / `zero_field!` macro usage |
| `mm/page_size.rs` | `cursor.map::<Size4Kb, _>(...)` invocation |
| `numfmt.rs` | `NumBuf::format_u64` buffer-usage pattern |
| `sync/cpu_local.rs` | `cpu_local!` macro syntax |
| `test_support/hermetic/macros.rs` | `hermetic_state!` macro syntax |

The same doctest binary also runs **10 `compile_fail` doctests** that
verify deliberate-misuse patterns are correctly rejected by the
compiler (e.g., trying to leak a `UFrame`'s mutable view past its
lifetime). Those count as the "10 passed" alongside the 20 ignored.
Promoting any of the 20 to `no_run` would require scaffolding
(user-typed placeholders, boot/init mocks) without catching real
bugs — net negative.

## Where `MIRI_FINDINGS.md` is

There isn't one. Per user direction during Phase 1B, any UB Miri
surfaces is **fixed inline in `slopos-ostd/` source** and reported in
the PR description. The findings during this phase were:

1. **Real UB**: `slopos-ostd/src/util/ptr_buf.rs::borrow_at_mut` could
   construct an unaligned `&mut [T]` if a caller passed a misaligned
   byte offset. Fix: `debug_assert!` on alignment + the in-tree test
   now uses a `#[repr(align(4))]` backing buffer.
2. **Soundness gap in a test (not in OSTD itself)**:
   `tests/kernel_sync.rs::refcell_u64_round_trips_across_threads` had
   four threads concurrently call `RefCell::borrow()`, racing on the
   non-atomic borrow counter. Benign on x86_64 but UB per the Rust
   memory model. Test ignored under Miri with an explanatory comment.
3. **Miri limitations** (not bugs, ignored under Miri): a handful of
   tests that depend on the real binary layout of `global_asm!`
   blocks or on `unsafe extern static` resolution.

## Background

- Asterinas KernMiri paper: <https://arxiv.org/abs/2506.03876>
- Miri repo: <https://github.com/rust-lang/miri>
- The plan: `plans/FRAMEKERNEL_PLAN.md` § B.
