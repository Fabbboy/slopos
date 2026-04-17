# SlopOS Stack-Safety Overhaul — Phase B

> **Status**: planning — big-bang work lives on branch `stack-safety-overhaul`.
> `develop` stays pristine until the branch lands as a single atomic merge.
>
> **Goal**: make the "large struct on a kernel stack" bug class unreachable
> **by design** — not by lint, not by discipline, not by review. Competing with
> and exceeding Linux / Asterinas / Redox on this specific axis.
>
> **Budget**: the user has explicitly authorised rewriting up to 50 kLoC to
> reach the correct end state. Phase B is expected at ~3–6 kLoC of
> mechanical type migrations plus one new workspace crate and one build
> script.

---

## 1. Context and prior art

Phase 1 (commit `44ec9a8`) replaced the kmalloc-backed task stack with a
VA-region-backed `KernelStack` allocator: each task gets 32 KiB usable
between two unmapped guard pages, modelled on Asterinas' CortenMM
(SOSP 2025). That correctly traps stack overflow at the guard page
instead of letting it silently corrupt the heap — which is how it should
always have been.

The catch: the old silent-overflow behaviour was masking a **large class
of pre-existing frame-size bugs** across the kernel. Objdump analysis of
`builddir/kernel.elf` (at the `55b9a8e` baseline) flagged functions with
stack frames in the 24–131 KiB range:

| Function                          | Frame     | Cause                                  |
| --------------------------------- | --------: | -------------------------------------- |
| `TcpShard::alloc_buffer_for`      | 131,600 B | holds two `TcpBufferPair` temporaries  |
| `ipv4::handle_rx`                 |  73,608 B | `Option<ReassembledPacket>` local (24 KiB `data`) |
| `TcpBufferPair::new`              |  65,792 B | returns `Self` containing two 32 KiB ring buffers |
| `TcpSendState::new`               |  65,584 B | same pattern                           |
| `TcpRecvState::new`               |  65,584 B | same pattern                           |
| `TcpShard::release`/`free_buffer` |  ~65 KiB  | moves `TcpBufferPair` by value         |
| `ReassemblyTable::insert`         |  57,616 B | builds `Fragment { data: [u8; 1500] }` + `ReassembledPacket::new` on stack |
| `syscall_process_list`            |  41,456 B | `[UserTaskEntry; MAX_TASKS]` local     |
| `unix_connect`                    |  33,376 B | `Box::new([0u8; UNIX_BUF_SIZE])` ×2    |
| `ReassemblyGroup::empty`          |  25,680 B | returns `Self` containing `[Option<Fragment>; 16]` |
| `LineDisc::new`                   |  20,640 B | returns `Self` containing 4 KiB edit buf + 8 KiB ring |

Phase A (commits `e93adea`, `f1b85ec`, `57d2667`, `9c4e857` on this branch)
retrofitted each of these with the idiomatic heap-direct pattern
(`vec![0u8; N].into_boxed_slice()`, `Box::<T>::new_zeroed().assume_init()`,
`LineDisc::new_boxed`, etc.). The kernel now boots the shell and survives
the PID 3 startup path at 32 KiB.

**Phase A is NOT the end state.** It fixes symptoms one type at a time.
Nothing prevents a future contributor from re-introducing the bug class
elsewhere. The user's explicit rejection: "patch-patch-patch is dogshit
— we need type-system enforcement".

### Research summary

Full research report lives in agent history; highlights:

- Rust NRVO is **best-effort, not guaranteed**. `compiler/rustc_mir_transform/src/nrvo.rs`'s
  `RenameReturnPlace` pass fires only on trivial MIR shapes and bails on
  branch merges, field-by-field init, drops, or sub-calls. rustc has no
  `-Wframe-larger-than` equivalent.
- Placement-new RFCs (RFC #809, #1228, #2884) all withdrawn or stalled.
  Today's **only** stable heap-direct primitives are `Box::new_uninit`
  and `Box::new_zeroed` — plus the out-of-tree [`pin-init`](https://rust.docs.kernel.org/pin_init/)
  crate that Rust-for-Linux uses in the upstream Linux kernel.
- **Linux (C)**: sidesteps via discipline. `kmalloc`/`kzalloc` everywhere,
  no value-returning struct constructors, `-Wframe-larger-than=1024` as a
  build-breaking CI gate since `CONFIG_VMAP_STACK` landed in 4.9 (2016).
- **Asterinas** (SOSP 2025 Best Paper): chose the opposite trade-off —
  512 KiB per-task stacks, relies on the OSTD framekernel discipline to
  keep `unsafe` out of the service crates. Not a type-level solution.
- **Redox**: avoids most of the problem by being a microkernel. TCP,
  VFS, line discipline all live in userspace scheme daemons with 8 MiB
  stacks. Not applicable to SlopOS's monolithic design.
- **Theseus** (OSDI 2020): single address space, `Arc<Mutex<T>>`
  everywhere, 22.5 % heap overhead. Academically clean, operationally
  expensive.
- **Tock / Hubris**: static preallocation only, no heap. Correct for
  MCUs, wrong for a general-purpose OS that hosts dynamic numbers of
  processes and sockets.
- **Rust-for-Linux `pin-init`**: the load-bearing, in-production
  mechanism for safe heap construction in the Linux kernel's Rust code.
  Runs on our nightly. Provides `impl PinInit<Self>` — a type-level
  construct that lets a function return "a recipe for constructing a
  `T` at a caller-provided heap slot", so the `T` value itself never
  exists on any stack.

### Why clippy is not the answer

User explicit: "clippy is dogshit". And technically correct — `clippy::large_stack_frames`
and `clippy::large_stack_arrays` are suppressible with `#[allow(...)]`,
fire only at `cargo clippy` time (not during `cargo build`), and carry
no type-level guarantee. They're a smoke alarm, not a firewall. We want
a firewall.

---

## 2. Target architecture: three orthogonal enforcement layers

The bug class is **cross-cutting** — no single Rust mechanism can close
it workspace-wide. We layer three independent enforcement primitives;
each has a hole the others fill.

### Layer 1 — Type system (pin-init + privacy)

Every large kernel type gets:

- Fields `pub(crate)` or private.
- `#[pin_data]` attribute from the `pin-init` crate.
- `#[derive(Zeroable)]` when all fields are zero-valid.
- Its only public constructor: `fn new() -> impl PinInit<Self, E>`.
  **Not** `fn new() -> Self`.
- Instances reached via `Box::pin_init(T::new())?` or
  `UniqueArc::pin_init(T::new())?` — both of which allocate the heap
  slot **first**, then run the initialiser against a pointer into that
  slot.

Result: from outside the module, `T { ... }` is a privacy error; `T::new()`
does not yield a `T` rvalue; there is no syntactic path from a caller to
a "large `T` on my stack". Watertight **per migrated type**.

Hole Layer 1 doesn't close: a contributor defines a new `pub struct
Thing { buf: [u8; 65536] }` with `pub fn new() -> Self` in a crate that
hasn't adopted pin-init yet. Layer 1 only protects opt-in types.
Closed by Layer 2.

### Layer 2 — Architectural boundary (`slopos-alloc` wrapper crate)

A new workspace crate `slopos-alloc` mediates **all** kernel heap
allocation:

```rust
// slopos-alloc/src/lib.rs (sketch)
#![no_std]
extern crate alloc;

pub use alloc::alloc::AllocError;

/// Kernel-wide pinned box.  Internal storage is `Pin<Box<T>>`.
/// The only public constructor takes a `PinInit<T>`.
pub struct PinBox<T: ?Sized> { inner: Pin<Box<T>> }
impl<T> PinBox<T> {
    pub fn pin_init<E>(init: impl PinInit<T, E>) -> Result<Self, E>
    where E: From<AllocError> { /* ... */ }
}

/// Heap-direct zeroed allocation, safe when `T: Zeroable`.
pub fn boxed_zeroed<T: Zeroable>() -> Result<PinBox<T>, AllocError> { /* ... */ }

/// Kernel-blessed Vec.  No by-value construction from a `[T; N]` literal.
pub struct KVec<T> { inner: alloc::vec::Vec<T> }
impl<T: Zeroable> KVec<T> {
    pub fn zeroed(len: usize) -> Result<Self, AllocError> { /* alloc_zeroed path */ }
    // ... push, iter, len, as_slice; no `From<[T; N]>`.
}

pub use pin_init::{pin_data, Zeroable, PinInit};
```

`slopos-alloc` is the **only** kernel crate with `alloc` in its
`[dependencies]`. Every other kernel crate depends on `slopos-alloc`
and uses its re-exports. Enforcement: a one-line CI check —
`grep -l '^alloc\|^alloc =' */Cargo.toml | grep -v '^slopos-alloc/'`
must be empty. Not a clippy lint; a dep-graph invariant.

Closes the hole Layer 1 leaves: even in a yet-to-be-migrated kernel
crate, `Box::new(bigexpr)` is unreachable because `alloc::boxed::Box`
isn't in scope. The contributor physically cannot write the
stack-materialising call.

Hole Layer 2 doesn't close: `let x: [u8; 65536] = [0; 65536];` as a
raw local. Array literals are built-ins; you cannot redefine or
restrict them at the crate level. Closed by Layer 3.

### Layer 3 — ELF backstop (`-Zemit-stack-sizes` + build-script check)

The rustc flag `-Zemit-stack-sizes` emits a `.stack_sizes` section in
the output ELF listing every function's worst-case frame size in bytes
(see [rustc-dev-guide: stack-sizes](https://doc.rust-lang.org/rustc/codegen-options/index.html#emit-stack-sizes)
and the [RFC 1974 proposal](https://github.com/rust-lang/rust/issues/57320)).

A post-link script (`scripts/check_stack_sizes.sh` or a `build.rs` on
the `kernel` crate) parses `builddir/kernel.elf`'s `.stack_sizes`
section and **fails the build** if any function exceeds the threshold.

This is strictly stronger than Linux's `-Wframe-larger-than` because:

- It inspects the **actual final machine code**, not a compile-time
  heuristic.
- It catches recursion through inlining, NRVO failures, stack arrays,
  or anything else that produces a large frame — regardless of its
  source pattern.
- It cannot be suppressed with `#[allow(...)]`. The check runs on the
  ELF, not on source.

Threshold plan:

- Initial: 8 KiB (permissive; matches Phase A achievable baseline).
- After Phase B.3 (full migration complete): 4 KiB.
- After Phase B.4 (final cleanup): 2 KiB.

### Why the combination is airtight

- Layer 1 blocks the idiomatic source pattern on types we've designed.
- Layer 2 blocks the `Box::new` / `Vec::new` / raw `alloc` path for
  everything else, migrated or not.
- Layer 3 is the universal backstop — **the build literally cannot
  produce a `kernel.elf` containing a function with a frame over the
  threshold.**

A contributor determined to reintroduce the bug would have to: (a)
implement their large type with pin-init (otherwise it can't be
constructed), **and** (b) launder their allocation through `slopos-alloc`
(otherwise they have no `Box::new`), **and** (c) still produce a bad
frame in the final binary (otherwise Layer 3 passes). All three are
unlikely in combination; any one of them failing blocks the merge.

---

## 3. Phased execution plan

Work lives on branch `stack-safety-overhaul`. `develop` stays at
`origin/develop` until the full branch merges atomically at the end of
Phase B.4. Intermediate commits on the branch are not expected to be
bisectable.

### Phase B.0 — Infrastructure (target: 2–4 days)

Goal: the three enforcement layers exist and produce meaningful errors
on current code.

1. Create `slopos-alloc/` workspace member.
   - `PinBox<T>`, `KBox<T: Zeroable>`, `KVec<T>`, `Zeroable` trait
     (or re-export `pin_init::Zeroable`).
   - `boxed_zeroed<T: Zeroable>() -> Result<PinBox<T>, AllocError>` —
     one centralised `unsafe` wrapping `Box::<T>::new_zeroed().assume_init()`.
   - `pin_init!` / `try_pin_init!` re-exports from the `pin-init` crate.
2. Add `pin-init` to workspace dependencies (vendor from Rust-for-Linux
   if crates.io version too stale; Linux tree pins a specific commit
   SHA).
3. Enable `-Zemit-stack-sizes` in `targets/x86_64-slos.json`.
4. Write `scripts/check_stack_sizes.sh`:
   - Parses `builddir/kernel.elf` `.stack_sizes` section
     (`llvm-objdump --section-headers` / a small Rust helper using
     `object` crate).
   - Threshold from `STACK_SIZE_THRESHOLD` env, default 8192.
   - Prints every offender, exits non-zero on the first.
5. Wire the check into `just build` as a post-link step (fails the
   recipe on overrun).
6. Add CI-equivalent grep: `scripts/check_alloc_dep.sh` verifies no
   kernel crate other than `slopos-alloc` names `alloc` as a
   dependency. Wire into `just build` and the justfile `check` recipe.

**Exit criterion**: `just build` still succeeds on Phase A code at
threshold 8192. Threshold-lowering and real `slopos-alloc` adoption
happens in later phases.

### Phase B.1 — Hot-subsystem migration (target: week 1–2)

The subsystems currently showing the biggest Phase A frames, migrated
to full pin-init + private fields + `slopos-alloc`:

- `net/src/tcp/buffer.rs` — `TcpSendState`, `TcpRecvState`,
  `TcpBufferPair`. Fields become private; `new()` returns
  `impl PinInit<Self>`; `alloc_buffer_for` uses `PinBox::pin_init`.
  `#[derive(Zeroable)]` on the ring-buffer-only states; chain-init
  for `effective_capacity` override.
- `net/src/tcp/table.rs` — `TcpShard::buffers` becomes
  `[Option<PinBox<TcpBufferPair>>; SLOTS_PER_SHARD]`.
- `drivers/src/tty/ldisc.rs` — `LineDisc`, `RawDisc` migrated to
  `#[pin_data] #[derive(Zeroable)]` + `impl PinInit<Self>`
  constructors. `LdiscKind` variants hold `PinBox<LineDisc>` /
  `PinBox<RawDisc>`.
- `drivers/src/tty/mod.rs` — `Tty` struct wrapped in `PinBox<Tty>` at
  the `TTY_SLOTS` boundary; fields private to the module.
- `net/src/unix_socket.rs` — `RingBuf`'s backing buffer becomes a
  pin-init-constructed `KVec<u8>::zeroed(UNIX_BUF_SIZE)` or a
  `PinBox<[u8; UNIX_BUF_SIZE]>` with a `Zeroable` impl. `pty_alloc`
  path updated.
- `net/src/reassembly.rs` — `Fragment`, `ReassembledPacket`,
  `ReassemblyGroup` migrated. `Fragment.data` stays `Box<[u8]>`
  allocated via `KVec::zeroed` + slicing.

Per-type `const _: () = assert!(size_of::<T>() <= N);` tripwires added.

**Exit criterion**: the migrated subsystems expose no `fn new() -> Self`
on any type larger than 128 bytes. The `.stack_sizes` post-link check
shows none of the migrated subsystems' functions over 4 KiB.

### Phase B.2 — Syscall, FS, and scheduler migration (target: week 2–3)

- `core/src/syscall/core_handlers.rs` — `syscall_process_list`,
  `syscall_cpu_info`, any `[T; MAX_*]` locals moved to `KVec<T>`.
- `core/src/syscall/fs/*` — argv/envp construction on `exec` path.
- `core/src/exec/` — ELF loader temporaries.
- `mm/src/process_vm.rs` — any large on-stack buffers in
  `process_vm_load_elf_data` etc.
- `fs/src/ext2/*` — the indirect-pointer scratch buffer
  (Phase A already boxed it; migrate to pin-init properly) and any
  similar patterns in the inode read/write paths.
- Task-structure scratch in `core/src/scheduler/task_struct.rs` and
  `task_table.rs`.

**Exit criterion**: all syscall handlers, FS, scheduler: no
`fn * -> T` where `size_of::<T>() > 128`. `.stack_sizes` threshold
tightened to 4 KiB for the whole kernel.

### Phase B.3 — Tail migration + dependency flip (target: week 3–4)

- Every remaining kernel crate (`boot`, `drivers/*`, `mm`, `fs`,
  `core`, `kernel`, etc.) switched from `alloc` to `slopos-alloc`.
  `grep` over `Cargo.toml` files must show only `slopos-alloc` on
  every kernel crate's dep list.
- The `scripts/check_alloc_dep.sh` grep becomes a hard build gate.
- A one-pass audit of the kernel binary: `llvm-objdump
  --section=.stack_sizes builddir/kernel.elf` must show max frame
  ≤ 4 KiB.
- `Phase A` interim fixes (current commits `e93adea`, `f1b85ec`,
  `9c4e857`, plus parts of `57d2667`) are either left as-is (if the
  Phase B migration replaced them cleanly) or deleted.

**Exit criterion**: every kernel function has frame ≤ 4 KiB. Every
kernel crate depends only on `slopos-alloc`, never on `alloc`
directly. No `pub fn * -> T` in kernel crates with `size_of::<T>() > 128`
(audited by AST walk in `scripts/check_return_types.sh`, run once for
the end-state verification — the check is disposable once migration is
complete because pin-init + privacy already forbids the pattern
structurally).

### Phase B.4 — Hardening and merge (target: week 4)

- `.stack_sizes` threshold tightened to **2 KiB** for kernel crates.
  Any remaining offender is investigated and fixed individually —
  these are typically tests or deep compiler-generated glue, not real
  frames.
- Documentation: `plans/STACK_SAFETY_PHASE_B.md` updated with the
  final numbers; `CLAUDE.md` gets a short section pointing to
  `slopos-alloc` as the only allocation surface for kernel code.
- Merge `stack-safety-overhaul` → `develop` as one squashed commit
  (or a clean rebased series if bisectability matters).
- `develop` gets the new enforcement invariants active from
  commit one.

**Exit criterion**: merge commit on `develop`; subsequent builds fail
on any of the three layers being violated; user validates
`ping google.com`, a real TCP connect, and a PTY open without stack
issues.

---

## 4. Out of scope

- **Clippy lints** (`large_stack_frames`, `large_stack_arrays`,
  `large_types_passed_by_value`). Explicitly rejected. Redundant under
  Layers 1 + 2 + 3.
- **Userland crates** (`userland`, `slibc`, `slop-protocol`,
  `ktesting`). Userland runs on 1 MiB stacks and the constraint
  doesn't apply. `slopos-alloc` is not required there; userland stays
  on `alloc`.
- **`just stack-audit` diagnostic recipe**. Kept as a developer
  shortcut for "show me the current worst offenders" but not part of
  enforcement.
- **Subsystem-level architectural changes** (move TCP to userland a la
  Redox, adopt Theseus-style `Arc<Mutex<T>>` globally, etc.). Not a
  stack-safety concern; separate roadmap item.

---

## 5. Success criteria (post-merge)

1. `kernel.elf` contains no function with a stack frame > 2 KiB. The
   build fails if that invariant is violated.
2. No kernel crate other than `slopos-alloc` has `alloc` in its
   `Cargo.toml` dependency list. The build fails if that invariant is
   violated.
3. Every large kernel type has `#[pin_data]` + private fields and
   exposes only `impl PinInit<Self>` constructors. (Enforced per-type
   by `pin-init`'s structural guarantees; verified once post-migration.)
4. `just boot-fast` brings up the shell, and `ping google.com`
   completes a DNS round-trip plus ICMP echo without kernel-stack
   faults.
5. `just test` reports 2391/2391 regression tests passing.
6. This document (`plans/STACK_SAFETY_PHASE_B.md`) is updated with the
   actual measured numbers from the final `.stack_sizes` dump.

---

## 6. Risks and open questions

- **pin-init crate sourcing**. crates.io has `pin-init = "0.0.9"` as of
  early 2026; the Linux kernel tree pins a newer, in-house version
  with `#[pin_data]` derive and `try_pin_init!` macro. Decide in
  Phase B.0: use crates.io (simpler) vs vendor from
  `rust-lang/rust-for-linux` (more features, matches upstream Linux's
  own usage). Leaning toward crates.io with a dependency-update plan;
  vendoring adds maintenance burden.
- **Unstable feature surface**. `pin-init` uses `allocator_api`,
  `new_uninit`, `ptr_metadata`, `lazy_cell` depending on version. All
  are available on our pinned nightly (`nightly-2026-03-22`) but
  restrict our ability to update the toolchain. Acceptable for a
  pre-alpha kernel.
- **Backward compatibility during migration**. The branch is
  expected to be broken at intermediate commits. This is fine because
  the branch exists exclusively for big-bang work and `develop`
  continues to accept hotfixes independently.
- **Test-code noise**. Some tests legitimately want to construct
  kernel types by value (unit-testing a line discipline with known
  inputs). pin-init supports this via a `Self::new()` → `impl PinInit`
  returned into a `Box::pin_init` inside the test — one extra line
  per test site. Not a blocker.
- **Non-Zeroable types**. A few types have non-zero invariants
  (bitflags with required bits, enums with non-zero discriminants,
  `NonNull`-bearing fields). Those go through `pin_init!` with
  explicit field initialisers rather than `#[derive(Zeroable)]` +
  `init::zeroed()`.
- **Dropped Copy/Clone on large types**. TCP buffer structs lost
  their `#[derive(Clone, Copy)]` in Phase A. Verify all callers
  accept `&`/`&mut` semantics. (Phase A commit audit shows no code
  actually uses the copy; this is safe.)

---

## 7. Change log

- 2026-04-17 — Plan created on branch `stack-safety-overhaul` after
  big-bang approval.
- 2026-04-17 — Phase B.0 infrastructure landed; threshold 8192 B,
  measured max frame 7288 B in a `core::array::IntoIter<_, 100>`
  instantiation inside `slopos-mm`. See §8 below.
- 2026-04-18 — Phase B.1 hot-subsystem migration complete.
  Threshold still 8 KiB; migrated subsystems' max frame 1 240 B
  (`ReassemblyTable::insert`). See §9 below.
- 2026-04-18 — Phase B.2 syscall / FS / scheduler migration
  complete; threshold tightened to **4 KiB**; production max frame
  4 056 B (`net::tcp::close`). See §10 below.
- (Phase B.3 completion date) — Full workspace flipped to
  `slopos-alloc`; `.stack_sizes` audit green.
- (Phase B.4 / merge date) — Merged to `develop`; threshold 2 KiB;
  invariants active.

---

## 8. Phase B.0 results (2026-04-17)

Infrastructure sub-phase complete. Three enforcement layers now stand up
on branch `stack-safety-overhaul`.

### What landed

- **`slopos-alloc` workspace crate** — wraps `pinned-init` (crates.io
  `0.0.10`, the Rust-for-Linux legacy branch) with `PinBox<T>`,
  `KBox<T: Zeroable>`, `KVec<T: Zeroable>`, and `boxed_zeroed<T>`. Two
  tight `unsafe` blocks (`boxed_zeroed` and `KVec::zeroed`), both guarded
  by `T: Zeroable`. No kernel crate consumes it yet; adoption begins in
  B.1.
- **`.stack_sizes` ELF backstop** — `-Zemit-stack-sizes` now actually
  reaches rustc (see "Deviation 1" below); post-link script
  `scripts/check_stack_sizes.sh` parses the section via toolchain
  `llvm-readobj --stack-sizes` and fails the build on any frame larger
  than `STACK_SIZE_THRESHOLD` (env var, default `8192`).
- **Alloc-dep gate** — `scripts/check_alloc_dep.sh` enumerates every
  kernel `Cargo.toml`, skips userland and `slopos-alloc`, and fails if
  any kernel crate declares a direct `alloc` dependency (section-aware;
  `[features]` stanzas are ignored).
- Both gates wired into `scripts/build_kernel.sh` so every build path
  (`just build`, `_iso-tests`, `boot-prod`) exercises them. New `just
  check` recipe allows standalone invocation.

### Measured baseline

Top 10 frames in the `dev`-profile `kernel.elf` at commit HEAD, from
`.stack_sizes`:

| Size (B) | Function (mangled) |
| -------: | :----------------- |
|   7 288 | `core::array::IntoIter<_, 100>::into_iter` instantiated in `slopos-mm` |
|   6 520 | `slopos_tests::xsave_tests::test_sse_multi_register_isolation` |
|   6 264 | `slopos_tests::xsave_tests::test_avx_xsave_xrstor_roundtrip` |
|   6 200 | `slopos_tests::xsave_tests::test_sse_xsave_xrstor_roundtrip` |
|   6 008 | `slopos_tests::tests_run_all` |
|   5 768 | `slopos_mm::process_vm::process_vm_load_elf_data` |
|   5 496 | `slopos_core::syscall::net_handlers::syscall_recvmsg` |
|   5 416 | `slopos_core::syscall::net_handlers::syscall_sendmsg` |
|   5 160 | `slopos_core::syscall::fs::poll_ioctl_handlers::syscall_poll` |
|   4 856 | `slopos_core::scheduler::task::task_table::init_task_manager{closure}` |

Max frame 7 288 B is below the 8 KiB plan target, so the default
threshold is kept at 8192 — no fallback needed.

### Deviations from the plan text (§3 Phase B.0)

1. **`-Zemit-stack-sizes` placement** — plan step 3 says "enable in
   `targets/x86_64-slos.json`". The flag was already live in
   `.cargo/config.toml:25` but never reached rustc: `scripts/build_kernel.sh`
   overrides the whole `RUSTFLAGS` layer via env var, which takes
   precedence over `[target.*]` in `config.toml` (cargo does **not**
   merge the two). Fixed by appending `-Zunstable-options
   -Zemit-stack-sizes` to the RUSTFLAGS export inside the build script.
   No `link.ld` patch was needed — rust-lld preserves the
   `SHT_LLVM_STACK_SIZE` section by default for our linker script
   layout.
2. **pin-init crate** — plan §6 referenced `pin-init 0.0.9` but crates.io
   now hosts two distinct projects: `pin-init 0.2.x` (nbdd0121's unrelated
   library) and `pinned-init 0.0.10` (the Rust-for-Linux legacy branch,
   which is what we want). Adopted `pinned-init 0.0.10` — it already
   provides `#[pin_data]`, `pin_init!`, `try_pin_init!`, `Zeroable`,
   `InPlaceInit`, and `Box::try_pin_init`. Migration to the new
   `pin-init` name will be a one-line rename when R4L completes its own
   migration.
3. **`unsafe` blocks in `slopos-alloc`** — plan §3 step 1 said "one
   centralised `unsafe`". Ended up with two (`boxed_zeroed` and
   `KVec::zeroed`) because `Vec<T>` cannot route through
   `Box::new_zeroed` without capacity bloat. Both cite the `T: Zeroable`
   bound identically.
4. **Stack-sizes parser tool** — plan step 4 mentions
   `llvm-objdump --section-headers`, which only reports a section's
   existence, not its contents. Switched to `llvm-readobj --stack-sizes`,
   which is the correct primitive and is already shipped by
   `llvm-tools-preview` in our pinned nightly.
5. **Alloc-dep gate parsing** — plan step 6 sketches a `grep | grep -v`
   one-liner. That matches `[features]` `alloc = [...]` stanzas (false
   positive — already tripped on `gfx`, `appkit`, `windowing` during
   initial testing). Rewrote as a section-aware `awk` script that only
   considers `[dependencies]` / `[*-dependencies]` / `[target.*.*dependencies]`.
6. **`extern crate alloc;` audit** — deferred to Phase B.3. Adding it in
   B.0 would emit noise without enforcement payoff until migration
   begins.
7. **Stack-sizes gate scope** — skipped on builds with the
   `kernel/builtin-tests` feature (`just test` / `_iso-tests`). Test
   builds compile in per-subsystem regression suites whose 8–12 KiB
   frames never reach the production kernel. The alloc-dep check still
   runs unconditionally. B.1+ will either migrate the test scaffolding
   (most of the offenders are `LineDisc::new` / `RawDisc::new` returning
   by value — the same pattern as the real drivers) or keep the
   test-build exemption permanent.

### Verification performed

- `just build` — green, both gates pass post-link.
- `just check` — green.
- `just test` — 2391/2391 pass; stack-sizes gate correctly skipped on
  test builds.
- `cargo +nightly-2026-03-22 build -p slopos-alloc --target targets/x86_64-slos.json
  -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem` — green.
- `STACK_SIZE_THRESHOLD=1024` on a fresh production build — correctly
  exits non-zero and lists 135 offenders, confirming the gate bites.
- Simulated `alloc = { workspace = true }` injection into
  `net/Cargo.toml` — `check_alloc_dep.sh` correctly fires, state
  restored clean afterward.

### Not done in B.0 (per scope)

- No kernel crate yet consumes `slopos-alloc`. Adoption is Phase B.1+.
- No threshold tightening below 8192. Phase B.1 drops to 4 KiB.
- `just stack-audit` recipe (pre-existing `sub rsp,0xNN` asm parser)
  left untouched — kept as a diagnostic shortcut per §4 of this plan.

---

## 9. Phase B.1 results (2026-04-18)

Hot-subsystem migration complete. Every subsystem originally flagged
in §1 now routes its heap allocations through `slopos-alloc`'s
pin-init surface; no `fn new() -> Self` on any migrated struct
produces a stack frame over 1.3 KiB.

### What landed

- **`slopos-alloc` API extensions**
  - Re-exports added: `pin_init`, `try_pin_init`, `init`,
    `try_init`, `init_from_closure`, `pin_init_from_closure`.
  - `PinBox::try_new(value: T)` helper for small-type callers where
    a brief stack materialisation is not a stack-safety concern
    (`Tty`, `ReassembledPacket`). Documented to prefer
    `PinBox::pin_init` / `PinBox::zeroed` for any large type.
- **`slopos-utils`** — `unsafe impl Zeroable for RingBuffer<T: Zeroable, N>`.
  Single blanket impl; enables every parent struct that embeds a
  `RingBuffer<u8, N>` to `#[derive(Zeroable)]` or
  `unsafe impl Zeroable`.
- **`net/src/reassembly.rs`** — `Fragment.data` migrated to `KVec<u8>`;
  `ReassembledPacket.data` migrated to `KVec<u8>` via `KVec::zeroed(MAX_REASSEMBLED_DATA)`;
  `ReassemblyGroup::empty()` stays `const fn` for static init, but
  the non-const `*group = Self::empty()` assignment pattern in
  `clear_group` / `init_group` is replaced with
  `reset_in_place()`, eliminating the ~700 B stack temp. Size
  tripwires (`Fragment ≤ 64 B`, `ReassembledPacket ≤ 64 B`,
  `ReassemblyGroup ≤ 1024 B`) planted at end of module.
- **`net/src/unix_socket.rs`** — `RingBuf.buf` migrated from
  `Option<Box<[u8]>>` to `Option<KVec<u8>>`; `alloc_buf()` now
  fallible, returning `Option<KVec<u8>>` (None surfaces as
  `-ENOMEM` in `unix_connect`). All `extern crate alloc;` / `use
  alloc::*;` removed from this file.
- **`drivers/src/tty/ldisc.rs`**
  - `pub const fn LineDisc::new() -> Self` **deleted** (12 KiB
    return-by-value hazard).
  - `new_boxed()` replaced with
    `new_pinned() -> Result<PinBox<Self>, AllocError>`, backed by
    `unsafe impl Zeroable for LineDisc` + `PinBox::<Self>::zeroed()`.
  - `new() -> PinBox<Self>` convenience (panics on OOM) for tests
    and boot-time callers — tests still write
    `let mut ld = LineDisc::new();` and get a `PinBox<LineDisc>`
    that derefs to `&mut LineDisc`.
  - Same triplet for `RawDisc`.
  - `LdiscKind::{NTty, Raw}` variants flipped from
    `Box<{LineDisc,RawDisc}>` to `PinBox<{LineDisc,RawDisc}>`.
  - `LdiscKind::from_id` now returns
    `Result<Option<Self>, AllocError>`.
- **`drivers/src/tty/{mod.rs,table.rs,pty.rs}`** — `Tty` wrapped in
  `PinBox<Tty>`; `TTY_SLOTS: [IrqMutex<Option<PinBox<Tty>>>;
  MAX_TTYS]`; `Tty::{new, new_pty_master, new_pty_slave}` return
  `Result<PinBox<Self>, AllocError>`; `tty_table_init` panics on
  OOM (`.expect`) for the two boot-time console slots; `pty_alloc`
  surfaces `AllocError` as `TtyError::OutOfMemory` → `-ENOMEM`.
  - New `TtyError::OutOfMemory` variant in `abi/src/tty_error.rs`
    mapping to `ERRNO_ENOMEM`.
- **`net/src/tcp/buffer.rs`** — fields `pub` → `pub(crate)`;
  `buf: Box<TcpBuffer>` → `buf: KBox<TcpBuffer>`; constructors take
  a `cap: usize` parameter (all call sites pass `TCP_BUFFER_SIZE`
  for now; the hook is there for the future `SO_SNDBUF` / `SO_RCVBUF`
  work in the TCP modernization roadmap) and return
  `Result<Self, AllocError>`. Added accessor methods (`send`,
  `send_mut`, `recv`, `recv_mut`, `ooo`, `ooo_mut`,
  `inflight`, `effective_capacity`, …) for callers outside the
  `tcp` module. Size tripwires: `TcpSendState ≤ 64 B`,
  `TcpRecvState ≤ 64 B`, `TcpBufferPair ≤ 256 B`.
- **`net/src/tcp/table.rs`** — `TcpShard.buffers` kept as the
  inline `[Option<TcpBufferPair>; SLOTS_PER_SHARD]` (see
  "Deviations" below). `alloc_buffer_for` now
  `-> Result<(), AllocError>` and takes the fallible inner
  `TcpBufferPair::new(TCP_BUFFER_SIZE)?`.
- **Test call sites** — 26 occurrences across
  `net/src/tests/{tcp_pcb_data_tests.rs, tcp_rst_validation_tests.rs, tcp_reasm_tests.rs}`
  migrated to `TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc")`
  etc. Test files in
  `drivers/src/tty_tests/{test_ldisc_core,test_ldisc_regression,test_pty_core}.rs`
  flipped from `LineDisc::new_boxed()` → `LineDisc::new()` (now
  returns `PinBox<LineDisc>`).

### Measured baseline

Top-10 frames in the production `kernel.elf` at this commit
(`llvm-readobj --stack-sizes`, production build — not the
`builtin-tests` variant):

| Size (B) | Function (demangled) |
| -------: | :------------------- |
|   7 288 | `core::array::IntoIter<_, 100>::into_iter` instantiated in `slopos-mm` |
|   6 520 | `slopos_tests::xsave_tests::test_sse_multi_register_isolation` |
|   6 264 | `slopos_tests::xsave_tests::test_avx_xsave_xrstor_roundtrip` |
|   6 200 | `slopos_tests::xsave_tests::test_sse_xsave_xrstor_roundtrip` |
|   6 008 | `slopos_tests::tests_run_all` |
|   5 768 | `slopos_mm::process_vm::process_vm_load_elf_data` |
|   5 496 | `slopos_core::syscall::net_handlers::syscall_recvmsg` |
|   5 416 | `slopos_core::syscall::net_handlers::syscall_sendmsg` |
|   5 160 | `slopos_core::syscall::fs::poll_ioctl_handlers::syscall_poll` |
|   4 856 | `slopos_core::scheduler::task::task_table::init_task_manager{closure}` |

Migrated-subsystem frames (post-B.1):

| Size (B) | Function |
| -------: | :------- |
|   1 240 | `ReassemblyTable::insert` (was 57 616 B at the §1 baseline) |
|     888 | `TcpShard::alloc_buffer_for` (was 131 600 B) |
|     760 | `unix_connect` (was 33 376 B) |
|     616 | `TcpBufferPair::new` (was 65 792 B) |
|     424 | `LineDisc::{read, receive_buf}` |
|     408 | `RawDisc::receive_buf` |
|     344 | `TcpShard::release` (was ~65 KiB) |
|     264 | `TcpSendState::enqueue` |
|     248 | `TcpShard::free_buffer_for` |

All migrated-subsystem frames **≤ 4 KiB**, satisfying the Phase B.1
exit criterion. The top-10 offenders all live in the still-
unmigrated syscall / mm / xsave-tests surface, which is Phase B.2
scope.

### Deviations from the plan text (§3 Phase B.1)

1. **`TcpShard.buffers` stays inline** — the plan text said flip to
   `[Option<PinBox<TcpBufferPair>>; SLOTS_PER_SHARD]`. After the
   `KBox<TcpBuffer>` migration `TcpBufferPair` is only ~48 B, so
   inlining four per shard is ~192 B of `.bss` per shard.
   Wrapping each in `PinBox` would cost an extra per-slot heap
   allocation and an indirection on every TCP hot-path access for
   zero stack-safety benefit (the big 32 KiB buffers live inside
   `KBox` already). Kept inline.
2. **`ReassemblyTable.groups` stays inline** — same argument as (1).
   `ReassemblyGroup` is ~700 B after the Box-slim-down that §1
   baseline's 25 680 B figure predates, and the whole
   `ReassemblyTable` is a single `.bss` static. The *actual* hazard
   was the `*group = ReassemblyGroup::empty()` stack temp, which is
   fixed by the new `reset_in_place()`.
3. **`ReassembledPacket` stays returned by value** — 32 B struct.
   Wrapping in `PinBox` would add indirection without a
   stack-safety win; the real hazard was the 24 KiB data buffer,
   which is now routed through `KVec::zeroed`.
4. **Macro re-export quirk** — `pub use pinned_init::pin_init;`
   through `slopos-alloc` is a name re-export; the macro's
   `$crate::__init_internal!` still resolves to `::pinned_init::`.
   Verified via `try_init!` usage path; kernel crates using
   `slopos_alloc::try_init!` would need `pinned-init` in their
   own `Cargo.toml` for the expansion to resolve. **Worked around
   by not using `try_init!` / `pin_init!` in migrated call sites —
   pre-allocate the inner `KBox<...>` / `KVec<...>` outside the
   `Self { ... }` literal and rely on plain `Self`-struct init
   for small outer types.** If a future type genuinely needs
   chain-init across allocation boundaries (TCP `effective_capacity`
   plumbing for SO_SNDBUF would), either add `pinned-init` as a
   direct dep there or forward the macros through slopos-alloc
   with a local helper macro.
5. **`LineDisc::new()` kept, repurposed** — plan said delete. In
   practice ~40 test sites call `LineDisc::new()` and rely on it
   producing a value that supports `&mut` access. Changed
   signature to `pub fn new() -> PinBox<Self>` (panic on OOM); the
   12 KiB hazard is gone because there is no return-by-value path,
   and tests continue to compile unchanged modulo their call to
   `new_boxed` which flipped to `new`. The fallible production
   constructor is `LineDisc::new_pinned() -> Result<PinBox<Self>,
   AllocError>`.
6. **Rust-analyzer / PinBox NLL interactions** — a few test sites
   hit E0502 "cannot borrow `*tty` as mutable because it is also
   borrowed as immutable" after the PinBox wrap. The compiler
   can't project auto-deref calls into disjoint field borrows the
   way it can for a direct `&mut T`. Fix: materialise
   `let tty: &mut Tty = guard.as_deref_mut().ok_or(...)?` once
   (instead of chaining `guard.as_mut().ok_or(...)?` + repeated
   auto-deref), or hoist the offending reborrow into a `let` —
   both done in-place in `drivers/src/tty/pty.rs:182` and
   `drivers/src/tty_tests/test_ldisc_noncanon.rs:435`.
7. **`slopos-alloc` no longer the only crate naming `alloc`** —
   still true at the Cargo.toml level (gate `check_alloc_dep.sh`
   passes). But `net` and `drivers` now enable
   `#![feature(allocator_api)]` to reach `AllocError` from
   slopos-alloc re-exports. Phase B.3 (the final
   `extern crate alloc` purge) is where this asymmetry disappears.

### Verification performed

- `just build` — green; both gates pass (`check_alloc_dep: OK`,
  `check_stack_sizes: OK`).
- `just test` — 2391/2391 pass (auto-shutdown fired clean).
- `llvm-readobj --stack-sizes builddir/kernel.elf | sort -rn` —
  top-10 dumped above; migrated-subsystem functions all ≤ 4 KiB.
- `just boot-log` — shell reaches PID 2 and the roulette starts
  drawing before the 15 s timeout; no panic, no kernel-stack
  faults on any of the migrated call paths.

### Not done in B.1 (per scope)

- Syscall / FS / scheduler frames — the top-10 still shows
  `syscall_recvmsg` (5 496 B), `syscall_sendmsg` (5 416 B),
  `syscall_poll` (5 160 B), `init_task_manager{closure}` (4 856 B),
  `process_vm_load_elf_data` (5 768 B). These are Phase B.2 scope;
  the stack-sizes threshold stays at 8192 B for this phase and
  drops to 4 KiB only after B.2 clears them.
- `extern crate alloc;` in non-`slopos-alloc` crates — left in
  place for B.1; removal is Phase B.3 ("full workspace flipped to
  `slopos-alloc`").
- `scripts/check_return_types.sh` AST walk — Phase B.3 end-state
  verification only.


---

## 10. Phase B.2 results (2026-04-18)

Syscall, FS, and scheduler migration complete. `scripts/check_stack_sizes.sh`
default threshold dropped from 8 192 B to **4 096 B**; every function
in the production `kernel.elf` now fits.

### What landed

- **`slopos-alloc` wiring** — `core`, `mm`, `fs` now declare
  `slopos-alloc = { workspace = true }`. `#![feature(allocator_api)]`
  added per-crate only at the first site that actually names
  `AllocError`, to avoid `unused_features` lint.
- **`init_process_vm`** (`mm/src/process_vm.rs`) — the
  `collect_active_pids() -> [u32; MAX_PROCESSES]` helper (the
  7 288 B `IntoIter<[u32; 0x100]>` offender per §1) was the sole
  caller; folded into `init_process_vm`'s loop so no `[u32; 256]`
  array ever materialises.
- **Scratch-buffer pattern (Pattern 1)** — 11 production call
  sites migrated from `let mut scratch = [0u8; 4096]` to
  `slopos_alloc::KVec::<u8>::zeroed(4096)`:
  - `core/src/syscall/net_handlers.rs` — `syscall_send`,
    `syscall_recv`, `syscall_sendto`, `syscall_recvfrom`,
    `syscall_sendmsg`, `syscall_recvmsg`
  - `core/src/syscall/ui_handlers.rs` — `syscall_clipboard_copy`,
    `syscall_clipboard_paste`
  - `core/src/syscall/process_handlers.rs` — `read_user_cstr_list`
    (`[u8; EXEC_MAX_ARG_STRLEN]` scratch, shared across argv
    iterations)
  - `fs/src/vfs_file_ops.rs` — `VfsFileOps::{read, write}`
  - `fs/src/pipe_file_ops.rs` — `PipeReadOps::read`,
    `PipeWriteOps::write`
  - `fs/src/fileio/mod.rs` — `LocalTtyOps::{read, write}`
  - `drivers/src/tty_file_ops.rs` — `TtyFileOps::{read, write}`
  - `net/src/unix_socket_file_ops.rs` — `UnixSocketFileOps::{read, write}`
  - `net/src/socket_file_ops.rs` — `SocketFileOps::{read, write}`
  Every migration surfaces allocation failure as `-ENOMEM` at the
  existing error-return boundary.
- **`Task::reset_in_place`** (`core/src/scheduler/task_struct.rs`) —
  replaces the 4 792 B `*slot = Task::invalid()` pattern in
  `task_table.rs` and `task_session.rs`. Takes `kernel_stack`
  explicitly (Option::take) to guarantee exactly-once drop of the
  owning `KernelStack`, then `write_bytes`-zeroes the rest of the
  struct **except** the `Option<KernelStack>` slot (Rust makes no
  layout guarantee that the all-zero bit pattern of an un-niched
  `Option<T>` is the `None` variant — writing zeros there would
  corrupt the discriminant and set up a later double-release).
  Non-zero sentinels (`INVALID_TASK_ID`, `TASK_PRIORITY_NORMAL`,
  the `/` in `cwd`) are set after the zero fill. A
  `size_of::<Task>() <= 8192` tripwire planted next to the
  existing `FPU_STATE_OFFSET` assertion.
- **`task_iterate_active` + `release_task_dependents`** — the
  `[Option<*mut Task>; MAX_TASKS]` stack buffer (~4 KiB) flipped to
  `KVec<usize>::zeroed(MAX_TASKS)`.
- **ELF validator out-param** — `ElfValidator::validate_load_segments_into(&mut [ValidatedSegment])`
  replaces the return-by-value API on the production path; test
  callers keep the old `validate_load_segments()` wrapper under
  `#[cfg(feature = "itests")]`. `ValidatedSegment` got an
  `unsafe impl Zeroable` + `ZERO` const. `process_vm_load_elf_data`'s
  `segments` array and `section_mappings` `[(u64,u64,u64); 16]` both
  now live in `KVec`.
- **TLB iterator** — `online_cpu_targets(exclude) -> ([usize; MAX_CPUS], usize)`
  replaced with `online_cpus(exclude) -> impl Iterator<Item = usize>`.
  `wait_for_acks` takes `impl IntoIterator<Item = usize>`; the four
  `flush_*` entry points (`flush_page`, `flush_range`, `flush_all`,
  `flush_asid`) pass the iterator directly — zero allocation, zero
  stack array. `targeted_flush_request` keeps its local `[usize; MAX_CPUS]`
  (2 536 B, under the 4 KiB ceiling) and now passes
  `targets[..n].iter().copied()` into `wait_for_acks`.
- **`syscall_poll`** — three parallel `[T; SELECT_MAX_FDS]` arrays
  (~3.5 KiB on the stack, rebuilt every poll iteration) lifted to
  the heap: `cached_revents` → `KVec<u16>`, `registered_ofis` →
  `KVec<u32>`, and `poll_fds` → `KVec<u8>` reinterpreted as
  `&mut [UserPollFd]` at use (UserPollFd = `i32 + u16 + u16`, all
  primitives, bit-wise zero-valid).
- **Additional struct-constructor migrations** needed to hit the
  4 KiB ceiling (not explicitly called out in §3 but covered by
  the same Pattern 2 rewrite):
  - `fs/src/pipe.rs::PipeSlot` — `buffer: [u8; PIPE_BUFFER_SIZE = 4096]`
    embedded directly. `*slot = PipeSlot::new()` in `alloc_slot`
    migrated to `PipeSlot::reset_in_place(ptr)` (`write_bytes` is
    sufficient — every field is zero-valued).
  - `fs/src/ext2/cache.rs::CacheEntry` — `data: Box<[u8; 4096]>`
    flipped to `KBox<[u8; 4096]>` so the `Box::new([0u8; 4096])`
    stack intermediate is gone. `BlockCache::new` becomes
    fallible; callers (`Ext2Fs::init_internal`, `Ext2Fs::from_parts`)
    propagate the error. A new `Ext2Error::OutOfMemory` variant
    carries it through `ext2_error_to_vfs`.
  - `net/src/route.rs::RouteTable::add` — `bucket.sort_by_key(|r| r.metric)`
    pulled in `core::slice::sort::driftsort_main` and its 4 KiB
    `AlignedStorage` scratch. Replaced with an insertion sort
    (bucket cap is 16).
- **Test-crate cfg gating** — `slopos_tests::{exception_tests,
  fpu_tests, xsave_tests}` modules gated on `#[cfg(feature =
  "builtin-tests")]`. `tests_run_all` body likewise gated; a
  zero-stub keeps the unconditional signature boot-drivers imports.
  Removes the ~6 KiB `TestRunSummary` stack frame from production.

### Measured baseline (just build, no `builtin-tests`)

Top 15 production frames post-B.2, all ≤ 4 096 B:

| Size (B) | Function |
| -------: | :------- |
|   4 056 | `slopos_net::tcp::close` |
|   3 576 | `SynSentState::on_segment` |
|   3 384 | `slopos_net::tcp::shutdown_write` |
|   3 320 | `task_lifecycle::init_task_context` |
|   3 208 | `syscall_select` |
|   2 936 | `slopos_net::socket::socket_send` |
|   2 808 | `syscall_ioctl`, `boot_step_interrupt_tests_fn`, `FpuState::new` (tied) |
|   2 776 | `SynRecvState::on_segment` |
|   2 712 | `FpuState::zero` |
|   2 648 | `TestRunSummary::default`, `syscall_poll` (tied) |
|   2 616 | `panic_handler_impl` |
|   2 536 | `fs::tests::build_ext2_image`, `tlb::targeted_flush_request` (tied) |

Compare against the Phase B.1 baseline (§9): every entry that was
then 4–6 KiB now sits below 4 KiB.

### Deviations from the plan text (§3 Phase B.2)

1. **Mystery 7 288 B frame was `collect_active_pids`, not a
   `[u32; 100]`.** The v0 symbol-mangling length field is
   lowercase-hex: `Amj100_` decodes as `[u32; 0x100 = 256]`, which
   matches `MAX_PROCESSES`. `collect_active_pids` was the sole
   caller; inlining the read loop into `init_process_vm`
   eliminated the helper entirely (no KVec needed — the loop can
   call `destroy_process_vm` directly without an intermediate array).
2. **`Task::reset_in_place` does not use `drop_in_place` on the
   whole struct**, contrary to the simplest "drop + re-write"
   approach. A first attempt that called `drop_in_place(this)`
   before `write_bytes` hit a double-release on `KstackSlot[0]`
   (the sentinel) during `init_task_manager`'s second invocation
   inside the test harness: `free_task_stacks` had already
   released the owning `KernelStack` and set `kernel_stack = None`,
   then `drop_in_place` inside `reset_in_place` ran the implicit
   Task::drop glue — whose Option<KernelStack>::drop found the
   field's bits zeroed by a preceding `write_bytes` and
   interpreted them as a `Some(KernelStack { slot: 0, … })`
   (layout is unspecified for un-niched `Option`). Final design:
   take `kernel_stack` via `Option::take` first, then
   `write_bytes` the two ranges on either side of the
   `kernel_stack` field, never touching its bit pattern.
3. **`tcp::close` stays at 4 056 B**. It builds a `DataState`
   (~3 KiB) via `Self::new(...)` into a local then
   `Box::new(ds)`-es it. Making that heap-direct requires either
   `pin-init` with `#[pin_data]` on `DataState` (Phase B.3 scope)
   or manual `Box::<DataState>::new_uninit` + field-by-field
   `ptr::write`. Left as-is: 4 056 B is below the 4 096 B gate.
4. **`net::route::RouteTable::add` insertion-sort swap**. The
   plan text didn't mention route sorting; measurement flagged a
   4 328 B `driftsort_main<RouteEntry, …>` frame from a single
   `bucket.sort_by_key(|r| r.metric)` call. Buckets are capped at
   `MAX_ROUTES_PER_BUCKET = 16`, so an open-coded insertion sort
   is strictly cheaper and avoids the generic sort's stack-resident
   `AlignedStorage`.
5. **ext2 `CacheEntry` field type flipped to `KBox`**, not just
   the allocation call. Keeping `Box<[u8; 4096]>` would have left
   the `Box::new([0u8; 4096])` stack hazard wherever a new entry
   is constructed (`BlockCache::new` at boot, and any future
   eviction/grow path). `KBox::zeroed` is inherently heap-direct.
6. **Test modules cfg-gated at module declarations**. Preexisting
   `boot_drivers.rs` imports from `slopos_tests` were not optional,
   so `slopos-tests` got linked into the production ELF even
   without `builtin-tests`, dragging `test_sse_*` and
   `tests_run_all` with their 6 KiB XSAVE/summary frames. Fix:
   gate the three suite modules + `tests_run_all`'s body with
   `#[cfg(feature = "builtin-tests")]`; provide a zero-stub
   `tests_run_all` for production. Alternative (skipping
   `slopos_tests::*` inside `check_stack_sizes.sh`) was rejected:
   the gate should fail on real leaks, and a crate-name filter
   dilutes that signal.

### Verification performed

- `just build` — green (both gates, 4 KiB threshold active).
- `just test` — 2391/2391 pass with no new regressions; panic
  handler never fires, auto-shutdown reports success.
- `llvm-readobj --stack-sizes builddir/kernel.elf | sort -rn | head` —
  top frame 4 056 B (`slopos_net::tcp::close`); no production
  frame above the 4 KiB gate.
- Production ELF boots the full shell path under `just boot-log`
  — `init` (PID 1) execs `shell` (PID 2) and hits the roulette
  within the 15 s timeout, no kernel-stack fault on any of the
  migrated call paths.

### Not done in B.2 (deferred to later phases)

- `net::tcp::close` / `DataState::from_syn_recv` — pin-init
  migration is Phase B.3 (full workspace `pin-init` / `PinBox`
  migration).
- `extern crate alloc;` purge — Phase B.3.
- `scripts/check_return_types.sh` AST walk — Phase B.4 end-state
  verification only.
- `.stack_sizes` threshold to 2 KiB — Phase B.4. Current top of
  4 056 B makes a 2 KiB gate unrealistic without the Phase B.3
  heap-direct migrations.

