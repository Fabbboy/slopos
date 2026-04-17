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
- (Phase B.0 completion date) — Infrastructure landed; threshold
  8 KiB, zero offenders.
- (Phase B.1 completion date) — Hot-subsystem migration complete.
- (Phase B.2 completion date) — Syscall/FS/scheduler migration
  complete; threshold tightened to 4 KiB.
- (Phase B.3 completion date) — Full workspace flipped to
  `slopos-alloc`; `.stack_sizes` audit green.
- (Phase B.4 / merge date) — Merged to `develop`; threshold 2 KiB;
  invariants active.
