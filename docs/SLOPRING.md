---
name: SlopRing — io_uring-Style Submission/Completion Ring Surface
description: Design for SlopOS's userspace async edge — a shared-memory SQ/CQ ring backed by the existing sync EventBus / WaitQueue substrate. Kernel side is sync; async lives in userspace.
status: design (Phase 3F) — no code yet; implementation is Phase 3G/3H
authors: Phase 3F design pass, grounded in the post-Phase-2 SlopOS sync substrate
phase: Framekernel Phase 3F (design subtasks 3F.1 / 3F.2 / 3F.3)
location: docs/SLOPRING.md (durable ABI/design spec; lives in docs/, not plans/)
---

<!--
  This spec is self-contained: it does not depend on any (ephemeral) plan file.
  The stable identifiers it uses — AD-N (architectural decisions), Inv. N (the
  ten OSTD soundness invariants), and R-N (risk-register IDs) — are framekernel
  terminology defined once in the framekernel architecture record; they are
  named here only as concept labels, never as a path into a plan document.
-->

# SlopRing — io_uring-Style Submission/Completion Ring Surface

> **This is the Phase 3F design document.** It exists *before any code* (the
> framekernel Phase-3F mandate: spec first). It closes subtasks **3F.1** (ring layout +
> submission/completion/cancellation/backpressure model), **3F.2** (per-opcode
> justification against an existing sync syscall path), and **3F.3** (threat
> model). Implementation lives in **3G** (`ring/` kernel crate, two syscalls,
> nine opcodes) and **3H** (`slibc-ring/` userland runtime + `nc` port). Nothing
> here builds anything; it pins the contract those phases must satisfy.

> **Load-bearing architectural constraint (AD-8 / AD-9 / R13).** The kernel side
> of SlopRing is **100% synchronous straight-line code**. There is no `async fn`
> anywhere in `ring/` or any other kernel crate; `scripts/check_no_kernel_async.sh`
> (wired in 3A.5) fails the build on one. Async is a property of the *userspace*
> runtime (3H) that drives this ring — never of the kernel. SlopRing is the
> "async edge" of *sync core, async edge*; it is the seam, not an async kernel.

---

## Table of Contents

1. [Goals and Non-Goals](#1-goals-and-non-goals)
2. [Why a Ring at All](#2-why-a-ring-at-all)
3. [The Sync Substrate SlopRing Sits On](#3-the-sync-substrate-slopring-sits-on)
4. [Ring Layout (SQ / CQ ABI)](#4-ring-layout-sq--cq-abi)
5. [Memory Model](#5-memory-model)
6. [The Two Syscalls](#6-the-two-syscalls)
7. [Submission Model](#7-submission-model)
8. [Completion Model](#8-completion-model)
9. [Blocking and the In-Flight Table](#9-blocking-and-the-in-flight-table)
10. [Cancellation](#10-cancellation)
11. [Backpressure and Overflow](#11-backpressure-and-overflow)
12. [Opcode Catalogue and Sync-Path Justification (3F.2)](#12-opcode-catalogue-and-sync-path-justification-3f2)
13. [Threat Model (3F.3)](#13-threat-model-3f3)
14. [Process Lifecycle: fork / exec / exit / signal](#14-process-lifecycle-fork--exec--exit--signal)
15. [Verification and Test Strategy](#15-verification-and-test-strategy)
16. [Open Questions Deferred to 3G/3H](#16-open-questions-deferred-to-3g3h)
17. [Glossary](#17-glossary)
18. [References](#18-references)

---

## 1. Goals and Non-Goals

### Goals

- Give userspace a **first-class async story** with the *shape* of Linux
  io_uring: a shared submission queue (SQ) and completion queue (CQ), batched
  submission, and blocking-or-polling completion harvest — all through two
  syscalls (`ring_setup`, `ring_enter`).
- Keep the kernel **strictly synchronous**. Each SQE is processed by
  straight-line sync code that reuses an *already-shipped* blocking syscall
  path; when that path would block, the existing `EventBus`/`WaitQueue`
  machinery arms the wait and the completion is posted from the existing wake
  path. No executor, no futures, no `async fn` in the kernel.
- **Opcode parity by construction**: every opcode produces the same observable
  result as its equivalent sync syscall under identical input, because it calls
  the *same code path* (3F.2 maps each opcode to its sync entry point; R12
  mandates a per-opcode parity test in 3G.6).
- Hold every framekernel soundness invariant **by construction**: ring memory is
  a `UFrame<RingMeta>` (Inv. 4 + Inv. 5), ring handles are generation-counter
  `Handle<T>` (AD-11), and no kernel reference into the ring outlives a single
  `ring_enter` call (3F.3).

### Non-Goals (this phase)

- **Not** an async kernel. SlopRing is the *opposite* of the retired
  async-first Phase 3 draft (retired in the framekernel architecture record's
  *Out of Scope* discussion).
- **Not** full Linux io_uring feature parity. No `IORING_SETUP_SQPOLL` kernel
  poll thread, no fixed files/buffers, no `IORING_SETUP_IOPOLL`, no linked SQEs,
  no `IORING_OP_*` breadth beyond the nine opcodes in § 12. Those are
  post-Phase-3 candidates; this phase keeps the surface small so it stays
  auditable and so the underlying sync syscall set stays frozen for Verus.
- **Not** new blocking primitives. Every opcode reuses an existing sync syscall
  path (3F.2); the complete-phase wait is the existing poll/select shape. The
  *only* new OSTD surface is a small volatile/atomic `UFrame` accessor (§ 3),
  required because reading user-writable ring memory with the current non-atomic
  byte interface would be a data race; it is bounded, audited, and KernMiri-gated
  (§ 15). Adding an opcode that needs a *new* kernel blocking primitive is out of
  scope — it would grow the audited surface without a parity anchor.
- **Not** a kernel-resident userspace runtime. The executor that turns
  completions into resolved futures is userland (3H), outside the
  `#![forbid(unsafe_code)]` kernel discipline.

---

## 2. Why a Ring at All

The post-Phase-2 SlopOS syscall surface is one-shot and synchronous: a thread
calls `fs_read`, the kernel parks it on a `WaitQueue` until data arrives, wakes
it, copies bytes, returns. One syscall, one blocking op, one thread parked. That
is correct and verifiable, but it forces userspace concurrency to mean *threads*:
N blocking operations need N parked threads.

io_uring's insight — and the reason every winning 2026 system uses *sync core,
async edge* — is that the syscall boundary can be **batched and decoupled** from
the blocking without making the kernel async:

- **Batched submission**: userspace fills K SQEs and crosses the syscall
  boundary once (`ring_enter(to_submit=K)`), amortising the SYSCALL/SYSRET cost.
- **Decoupled completion**: a submission that would block does not park the
  submitting thread; the submit phase records an in-flight *row* and returns. The
  submitter keeps issuing work. Completions are harvested later — poll the CQ for
  ready ops, or `ring_enter(min_complete=M)` to block once and drive the blocked
  ops to completion (§ 7.1, § 8.3).

Crucially, **none of this requires the kernel to be async** — nor any new wake
mechanism. The blocking itself is the *existing* `poll`/`select` wait shape
(§ 3, § 7.1): when the user asks to harvest, the kernel registers *the calling
task* on each in-flight op's resource queue, re-probes, and blocks that task —
exactly what `poll(2)` does today. There is **no** `(ring, op)` wake callback and
**no** CQE posted from a producer's wake path (the substrate cannot express
either — § 7.1 explains why, and an early draft that assumed it could was wrong).
The async-ness (one userspace thread juggling many in-flight ops) lives entirely
in the userspace runtime. The kernel never holds a suspended computation across a
yield; it holds a *table row* describing a pending op, which is plain data, and
binds the wait to the harvesting task only for the duration of a blocking
`ring_enter`.

---

## 3. The Sync Substrate SlopRing Sits On

SlopRing reuses these post-Phase-2 primitives. The design's credibility is that
it adds an *interface* and reuses the existing *blocking* mechanism (the
poll/select wait shape) **unchanged** — the one genuinely new OSTD primitive is a
bounded, audited volatile/atomic `UFrame` accessor (last row), needed only
because concurrent user-writable ring memory cannot be read through the current
non-atomic byte interface without a data race (§ 5.3). No new *blocking* or *wake*
mechanism is invented; the complete-phase is the existing `poll`/`select` loop
with the calling task as the waiter (§ 7.1).

| Primitive | Location | Role in SlopRing |
|---|---|---|
| `EventBus` / `BUS` (typed `KernelEvent` → `WaitQueue`; the enum itself lives in `abi/src/event.rs`) | `slopos-ostd/src/sync/event_bus.rs` (enum in `slopos-abi`) | In the *complete* phase of `ring_enter` the kernel registers the **calling task** (poll/select shape, `subscribe_current`) on each in-flight op's `KernelEvent`; the existing producer's `BUS.publish(ev)` wakes that task, which re-probes its in-flight set and posts CQEs. The wake unit is a *task*, never a `(ring, op)` callback (§ 9). |
| `WaitQueue` (`enqueue_current`/`remove_current`, `wake_all`/`wake_one`); `subscribe_current`/`unsubscribe_current` on `BUS`; `block_current_task_with_timeout` (the multi-queue block, in `sched`, exposed via `kernel-services::driver_runtime`) | `slopos-ostd/src/sync/wait_queue.rs`, `event_bus.rs`; `sched/src/sleep.rs` | The block/wake mechanism. The harvest block is the **multi-fd poll/select shape**: `subscribe_current` on N deduped queues → `block_current_task_with_timeout(deadline)` → reprobe on wake → post-block `has_pending_signal()` check (the exact loop in `core/src/syscall/fs/poll_ioctl_handlers.rs`). Single-queue `Subscription::wait_event` is **not** usable here — it binds one queue. |
| `has_pending_signal()` probe (the `wait_event` predicate is augmented with it so a `kill()` short-circuits the park → `EINTR`) | impl `runtime_has_pending_signal` in `core/src/driver_hooks.rs`, declared `kernel-services/src/driver_runtime.rs`; **called only from `net/src/socket.rs` (AF_INET) today** | `ring_enter(min_complete>0)`'s harvest loop does its own post-block `has_pending_signal()` check, so 3G must wire the probe into *that* loop explicitly — it is **not** inherited for free. Note the AF_UNIX wait path (`net/src/unix_socket/`) does **not** currently call the probe (§ 14); 3G's ring completion loop carries the probe regardless of opcode family. |
| `Frame<M: AnyFrameMeta>` / `UFrame<M: AnyUFrameMeta>` (trait in `uframe.rs`; the real `AnyUFrameMeta` exemplar is `AnonymousMeta`, **not** `PacketMeta`/`PageCacheMeta`, which are `AnyFrameMeta`-only) | `slopos-ostd/src/mm/frame.rs`, `uframe.rs` | Ring memory is `Frame<RingMeta>` exposed to userspace as `UFrame<RingMeta>` (byte-copy only; no `&T` over it). New dual-trait `RingMeta` meta type modelled on `AnonymousMeta` (§ 5.2). |
| `VmSpace::cursor_mut(range).map::<S, M>(uframe, prop)` | `slopos-ostd/src/mm/vm_space.rs` | The *only* way ring pages get into the user address space — same path `mmap(MAP_SHARED)` already uses via `process_vm_mmap_shared`. |
| `process_vm_mmap_shared` / `memfd_get_phys` (shared-frame mapping precedent) | `mm/src/process_vm.rs`, `mm/src/memfd.rs` | The existing template for "one set of physical frames, mapped writable into both kernel (HHDM) and user (`cursor_mut`)". SlopRing's ring mapping follows this exact pattern. |
| `HandleTable<T>` / `Handle<T>` (generation-counter handles, Phase 2H) | `slopos-ostd/src/handle.rs` | Per-ring kernel state is a `HandleTable` row; a stale `RingFd` returns a typed `HandleError`, never UB (3G.3). |
| `FileOps` / `FileKind` (open-file description dispatch) | `abi/src/file_ops.rs`, `fs/src/fileio/` | A ring is an open file (new `FileKind::Ring`), so it inherits fd lifecycle: `close`, `dup`, fork-inheritance, exec teardown — no new fd machinery (§ 14). |
| `file_read_fd` / `file_write_fd` (`IoBufRead`/`IoBufWrite`) | `fs/src/fileio/fdops.rs` | The `OP_READ`/`OP_WRITE` sync entry points (§ 12). |
| `socket_recv`/`socket_send`/`socket_accept` (AF_INET), `unix_recv`/`unix_send`/`unix_accept`/`unix_recvmsg`/`unix_sendmsg` (AF_UNIX) | `net/src/socket.rs`, `net/src/unix_socket/mod.rs` | The socket-opcode sync entry points (§ 12). |
| `file_poll_register_fd` (poll/select registration) | `fs/src/fileio/poll.rs` | The `OP_POLL_ADD` sync entry point (§ 12). |
| `define_syscall!` (typed-argument handler macro) + `SyscallContext` | `core/src/syscall/macros.rs`, `context.rs` | `ring_setup` / `ring_enter` are defined with this macro like every other syscall — both sync. |
| `UserSlice<T>` / `UserBytes` / `UserPtr<T>` (validated user pointers) | `slopos-ostd/src/user/ptr.rs` | Every user-supplied address inside an SQE is re-validated through these before any access (3F.3). |

**The new mechanism is small but real — and there are *two* pieces, not one.**
The in-flight table, the SQ/CQ shared-memory layout, and the per-ring kernel
lock (§ 4, § 6.2, § 9) are pure safe-Rust data structures in the `ring/` crate.
*Blocking* is fully borrowed — the complete-phase of `ring_enter` is the
existing poll/select wait shape (`subscribe_current` on N queues → re-probe →
block the caller via `block_current_task_with_timeout` → reprobe → unregister;
the `core/src/syscall/fs/poll_ioctl_handlers.rs` loop verbatim). **But one new
OSTD primitive is required**: a *volatile/atomic* byte-and-`u32`
accessor on `UFrame` (`load_u32_acquire` / `store_u32_release` /
`copy_out_volatile` / `copy_in_volatile`). Today `UFrame` exposes only
non-atomic `read_bytes`/`write_bytes`/`read_pod`/`write_pod`; reading
user-writable ring memory with those while userspace concurrently writes is a
data race (UB) and emits no fence, so the acquire/release ABI of § 4.2 is
*unsatisfiable* through the current surface. This accessor is `unsafe` raw-memory
code → it lives in OSTD, carries a `// SAFETY:` note, and is added to 3G's scope
with its own KernMiri obligation (§ 15). It is a *bounded, audited* TCB addition
(a handful of lines), not a mechanism that grows with features.

---

## 4. Ring Layout (SQ / CQ ABI)

The ABI mirrors Linux io_uring closely enough that the mental model transfers,
but is trimmed to SlopOS's nine opcodes and 64-bit-clean fixed-width fields. All
structures are `#[repr(C)]`, defined once in `abi/` (the single source of truth,
imported by both kernel `ring/` and userland `slibc-ring/`, exactly like
`abi/src/syscall/numbers.rs`).

### 4.1 Shared region overview

A ring is one contiguous shared region (a small power-of-two number of 4 KiB
frames; see § 5). It is carved into four sub-areas with fixed offsets recorded in
a header so userland never hard-codes layout:

```
+-------------------+  offset 0
|  RingParams hdr   |  immutable after setup: offsets, ring sizes, feature flags
+-------------------+
|  SQ control       |  sq_head (kernel-owned), sq_tail (user-owned), sq_mask, sq_dropped
+-------------------+
|  CQ control       |  cq_head (user-owned), cq_tail (kernel-owned), cq_mask, cq_overflow
+-------------------+
|  SQE array        |  `entries` x Sqe (64 bytes each)
+-------------------+
|  CQE array        |  `entries` x Cqe (16 bytes each)
+-------------------+
```

Unlike Linux (which has an SQ *indirection array* of indices into a separate SQE
array, a historical artifact), SlopRing uses a **direct SQE array**: SQ slot `i`
*is* SQE `i`. Simpler, one fewer indirection to validate, and we have no
backwards-compat obligation. The CQ is likewise a direct CQE array.

### 4.2 Head/tail index discipline

Both queues are classic single-producer/single-consumer ring buffers indexed by
free-running `u32` counters, masked into the array by `& mask` (so `mask =
entries - 1`, `entries` a power of two). Ownership of each index is split so that
**no index is written by both sides** — the core of the lock-free contract:

| Index | Written by | Read by | Meaning |
|---|---|---|---|
| `sq_tail` | userspace | kernel | producer cursor: userspace bumps after filling SQE(s) |
| `sq_head` | kernel | userspace | consumer cursor: kernel bumps after consuming SQE(s) |
| `cq_tail` | kernel | userspace | producer cursor: kernel bumps after posting CQE(s) |
| `cq_head` | userspace | kernel | consumer cursor: userspace bumps after harvesting CQE(s) |

The queue is empty when `head == tail`; full when `tail - head == entries`. All
arithmetic is wrapping `u32`; `(tail - head) <= entries` is an invariant the
kernel **re-derives and clamps**, never trusts (§ 13).

**Memory ordering.** Userspace must complete all SQE field writes *before* the
store that publishes `sq_tail` (release); the kernel does an acquire load of
`sq_tail` before reading any SQE. Symmetrically the kernel completes CQE field
writes before the release store to `cq_tail`; userspace acquires `cq_tail` before
reading a CQE. This is the same store-before-publish / acquire-before-read shape
the `EventBus` atomic-publish rule documents (`event_bus.rs` "Atomic-publish
contract").

**Crucially, this ordering must be expressed through atomic/volatile accesses,
not the plain byte-copy interface.** Ring memory is user-writable *concurrently*
with the kernel reading it, so a non-atomic, non-volatile read (`read_pod` /
`read_bytes`) racing a userspace write is a data race — UB in the Rust/LLVM
abstract machine *regardless* of the value-level snapshot argument (§ 13.3), and
`core::ptr::read` emits no fence so LLVM may reorder the SQE-body reads relative
to the `sq_tail` read even on x86. The kernel therefore reads indices via
`UFrame::load_u32_acquire`, publishes its own indices via
`UFrame::store_u32_release`, and copies SQE/CQE bodies via
`copy_out_volatile`/`copy_in_volatile` — the new OSTD accessors (§ 3). On x86_64
(AD-13) the index ops are a `MOV` plus an acquire/release *compiler* fence and
the body copies are volatile `memcpy`; the ABI is specified in acquire/release
terms so the contract holds independent of arch. Linux uses
`READ_ONCE`/`smp_load_acquire` here for exactly this reason.

### 4.3 `RingParams` (header, immutable post-setup)

Returned to userland by `ring_setup` (written into the head of the shared region;
also copied to a user-supplied `RingParams*` out-pointer so userland learns the
offsets without first mapping). Fields (all `u32` unless noted):

- `sq_entries`, `cq_entries` — power-of-two slot counts (`cq_entries =
  2 * sq_entries`, matching Linux's default headroom so a full SQ batch plus
  in-flight async completions rarely overflows the CQ).
- `sq_off_*` / `cq_off_*` — byte offsets of each control field and the SQE/CQE
  arrays within the region.
- `flags` — feature bits negotiated at setup (e.g. `SLOPRING_FEAT_SINGLE_MMAP`
  = the whole region is one mapping; this is the only mode in 3G).
- `region_bytes: u64` — total mapping length, so userland `mmap`s exactly this.

### 4.4 `Sqe` — Submission Queue Entry (64 bytes, `#[repr(C)]`)

64 bytes matches Linux io_uring's `struct io_uring_sqe`, keeps each SQE on its own
half cache line, and leaves room without versioning churn.

```rust
#[repr(C)]
pub struct Sqe {
    pub opcode: u8,        // OP_* — see § 12
    pub flags: u8,         // SQE flags (reserved=0 in 3G; room for future LINK etc.)
    pub _pad0: u16,
    pub fd: i32,           // target fd (file/socket/pipe), or -1 for OP_NOP/OP_TIMEOUT
    pub off: u64,          // file offset (OP_READ/OP_WRITE), or timeout ns (OP_TIMEOUT)
    pub addr: u64,         // user VA of the data buffer / msghdr / sockaddr (validated)
    pub len: u32,          // byte length of buffer at `addr`
    pub op_flags: u32,     // per-opcode flags (poll mask, recv/send flags, cancel flags)
    pub user_data: u64,    // opaque cookie: copied verbatim into the matching CQE
    pub addr2: u64,        // secondary VA (e.g. accept's sockaddr-len out-ptr)
    pub _resv: [u64; 1],   // reserved, must be zero
}
const _: () = assert!(core::mem::size_of::<Sqe>() == 64);
```

`user_data` is the correlation token: userspace chooses it (typically a pointer
to its future/continuation), and it is echoed unmodified into the CQE so the
runtime can route the completion. The kernel treats it as opaque bytes.

### 4.5 `Cqe` — Completion Queue Entry (16 bytes, `#[repr(C)]`)

```rust
#[repr(C)]
pub struct Cqe {
    pub user_data: u64,    // echoed from the originating SQE
    pub res: i32,          // result: >=0 success (e.g. bytes), <0 negated errno
    pub flags: u32,        // CQE flags (e.g. SLOPRING_CQE_F_MORE — unused in 3G)
}
const _: () = assert!(core::mem::size_of::<Cqe>() == 16);
```

`res` carries the *exact* value the equivalent sync syscall would return: byte
count on success, negated errno on failure (`-EAGAIN`, `-ECANCELED`, `-EINTR`,
`-EFAULT`, …). This is the literal mechanism of opcode parity (§ 12): the opcode
handler calls the sync path and stuffs its return into `res`.

---

## 5. Memory Model

### 5.1 Ownership and mapping

The ring region is a set of `Frame<RingMeta>` allocated by `ring_setup` through
the standard `slopos-ostd` frame allocator (Inv. 1: the frames come from unused
memory). **Ownership stays with the kernel-side ring object** (held in the
`HandleTable` row, § 9), for the ring's whole lifetime. The frames are exposed
two ways:

1. **To the kernel**, through the HHDM (higher-half direct map) — the kernel
   reads SQEs and writes CQEs via ordinary byte access to the HHDM virtual
   address of each frame. This is how the kernel already touches any owned
   `Frame<M>`.
2. **To userspace**, by mapping the *same* physical frames into the calling
   process's `VmSpace` via `cursor_mut(range).map::<Size4KiB, RingMeta>(uframe,
   prop)` with read+write user permissions. This is precisely the
   `process_vm_mmap_shared` path that `memfd` + `mmap(MAP_SHARED)` already uses
   (`mm/src/process_vm.rs`), so SlopRing introduces no new mapping mechanism.

Because the same frames are writable from both sides, the SQ/CQ are genuine
shared memory — userland writes SQEs and reads CQEs without a syscall; the
syscall (`ring_enter`) is only the *doorbell* and the *block point*.

### 5.2 `RingMeta` — the new frame metadata type

A new `AnyFrameMeta` + `AnyUFrameMeta` type alongside `PacketMeta` /
`PageCacheMeta` in `slopos-ostd/src/mm/frame.rs`:

```rust
/// Frame metadata for a SlopRing shared region page. Carries the owning
/// ring's generation-handle bits so a stray mapping can be traced to its
/// ring; payload is one atomic, well within MAX_META_SIZE (16 B). on_drop
/// returns the physical frame to the registered allocator (no leak on last
/// Drop). Modelled on `AnonymousMeta` (the existing `AnyUFrameMeta` exemplar
/// in `uframe.rs`), not on `PacketMeta`/`PageCacheMeta` (those are
/// `AnyFrameMeta`-only). Defined in `frame.rs` because `on_drop` calls the
/// module-private `return_frame_to_allocator`.
#[derive(Default)]
pub struct RingMeta {
    pub ring_handle_bits: AtomicU64,   // AtomicU64 = 8 B, fits MAX_META_SIZE = 16
}
unsafe impl AnyFrameMeta for RingMeta {
    fn on_drop(&mut self, paddr: Paddr) { return_frame_to_allocator(paddr); }
}
// `AnyUFrameMeta` (trait defined in `uframe.rs`) certifies the frame is
// untyped from the kernel's view — byte-copy only, never `&T` over it.
unsafe impl AnyUFrameMeta for RingMeta {}
```

`AnyUFrameMeta` is the load-bearing marker: it certifies the frame is *untyped*
from the kernel's perspective — the kernel must never form a `&Sqe` / `&mut Cqe`
Rust reference into ring memory, only byte-copy through the `UFrame` /
`UserSlice` interfaces. See § 13.2 for why.

### 5.3 No `&T` over the ring — the central soundness rule (Inv. 4 + AD-3)

This is the single most important memory rule and the one the Theseus bug class
(framekernel decision **AD-3**) is about. Userspace can mutate ring bytes
*concurrently* with the kernel reading them (it shares the mapping and may have
another thread running). Therefore the kernel **must not** hold a `&Sqe` or
`&mut Cqe` — a live Rust reference asserts the pointee is immutable (or
exclusively owned) for the reference's lifetime, which is false for shared,
user-writable memory. That is instant UB even with no `unsafe` visible at the
use site.

The rule, enforced by construction:

- **Read an SQE**: byte-copy the 64 bytes *out* of the ring into a kernel-stack
  `Sqe` (a private copy) via the `UFrame` byte interface, then operate only on
  the copy. Validation and dispatch see the snapshot; the ring can change
  underneath with no effect (TOCTOU-closed; § 13.3).
- **Write a CQE**: build a 16-byte `Cqe` on the kernel stack, then byte-copy it
  *into* the ring slot. The kernel never reads back what it wrote.
- **Indices**: `sq_tail` / `cq_head` (user-written) are read via
  `UFrame::load_u32_acquire` (a single volatile acquire load), never via a
  `&AtomicU32` into the ring. The kernel keeps its *own* `sq_head` / `cq_tail`
  in the kernel-side ring object (the source of truth for control decisions) and
  mirrors them into the shared page on write via `UFrame::store_u32_release`.
  These volatile/atomic accessors are the new OSTD surface (§ 3); plain
  `read_pod` would be a data race against the concurrent user writer.

This keeps ring memory in the `UFrame` "byte-copy only" regime end-to-end, so
Inv. 4 (sensitive memory untouchable by clients — here, the inverse: client
memory never aliased as a typed kernel reference) holds with no proof obligation
beyond the `AnyUFrameMeta` boundary that 3D already verified for `VmSpace`.

### 5.4 Sizing and the 2 KiB stack ceiling

The SQE snapshot (64 B) and CQE (16 B) are tiny; the in-flight table row (§ 9) is
a handful of words. The opcode handlers reuse existing syscall code paths whose
stack frames already pass `scripts/check_stack_sizes.sh` (2 KiB / Inv. 5'). The
ring crate adds no large stack buffers — bulk I/O buffers live in user memory
(`addr`/`len`), copied by the *existing* `file_read_fd` / `socket_recv` paths
that already stage through `IO_STAGING_SIZE` (4 KiB heap, not stack). 3G must
keep the per-SQE processing frame under the ceiling; the gate enforces it.

---

## 6. The Two Syscalls

Exactly two new syscalls, both defined with `define_syscall!` and **both sync**
(threaded through the existing dispatch path in `core/src/syscall/`; no executor
turn). Numbers are appended after the current max (`SYSCALL_RUN_USERLAND_TESTS
= 156`). Note `SYSCALL_TABLE_SIZE` is already `158`, so slot `157` is free
below it; the two new syscalls take `157`/`158` and `SYSCALL_TABLE_SIZE` is
bumped to `≥ 159` in 3G.2.

### 6.1 `ring_setup(entries: u32, params: *mut RingParams) -> i32`

- Validates `entries` (power of two, `1 <= entries <= SLOPRING_MAX_ENTRIES`;
  proposed cap 4096 so the largest region stays a few hundred KiB).
- Allocates the `Frame<RingMeta>` region (§ 5), zero-fills it, writes the
  `RingParams` header.
- Creates the kernel-side ring object, inserts it into the per-process ring
  `HandleTable`, and opens an fd of `FileKind::Ring` referring to it (§ 14).
- Maps the region into the caller's `VmSpace` (read+write user).
- Copies the populated `RingParams` (including the user VA the region was mapped
  at and all offsets) out to the user `params` pointer via `UserPtr`.
- Returns the ring fd (`>= 0`) or a negated errno.

**On the `flags` argument.** The framekernel 3G.2 subtask sketch writes
the signature as `ring_setup(entries, flags)`. SlopOS is pre-alpha with no ABI
compatibility obligation, so 3G defines the honest signature `ring_setup(entries:
u32, params: *mut RingParams) -> i32` directly in `numbers.rs` rather than
type-punning a `*mut RingParams` through a slot named `flags` (an earlier draft
proposed that pun; it is rejected here as needless obfuscation). Feature flags,
when they arrive, live in a reserved field *inside* `RingParams`, so the syscall
arity never churns. (This resolves former open question § 16.3.)

### 6.2 `ring_enter(ring_fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> i32`

The doorbell + harvest call. Synchronous from the kernel's view. **The
bookkeeping mutations run under the per-ring lock** (§ 6.3) so two threads that
race `ring_enter` on the same `ring_fd` cannot corrupt the indices / in-flight
table; the lock is *released before the task actually sleeps* in the complete
phase (§ 6.3) and re-taken to post deferred CQEs and advance indices:

1. Resolve `ring_fd` → ring object via the fd table and the `HandleTable`
   (stale/foreign fd → `-EBADF`, never UB; § 9, § 14). Acquire the per-ring lock.
2. **Submit phase**: acquire-load the user's `sq_tail`; process up to
   `n = min(to_submit, sq_entries, sq_tail - sq_head)` SQEs starting at the
   kernel's `sq_head` (§ 7; the `sq_entries` clamp is load-bearing — § 13.6).
   Each SQE either completes inline (post CQE now) or records an in-flight row
   (§ 9). Advance `sq_head`. Remember `n_submitted = n`.
3. **Complete phase** (`min_complete > 0`): this is a **`poll()` over the
   in-flight set**, not a wait on a private completion queue (§ 8.3, § 9). The
   caller registers itself (`subscribe_current`) on every *distinct* in-flight
   `KernelEvent` queue, re-probes all in-flight ops posting any now-ready CQEs,
   and — if still short — blocks via `block_current_task_with_timeout(deadline)`,
   re-probing on each wake and checking `has_pending_signal()`, until
   `available_cqes() >= min_complete`, a signal, or the deadline (§ 7.1, § 8.3).
   On exit it unregisters from every queue (`remove_current` +
   `file_poll_clear_registrations`). If `min_complete == 0`, skip this phase
   entirely (pure submit / poll mode).
4. Drop the per-ring lock. Return value: **if `n_submitted > 0`, always return
   `n_submitted`** (even when the complete phase was interrupted by a signal —
   never discard a submission, or userspace double-submits; § 13 M-class /
   Linux semantics). Return `-EINTR` only when nothing was submitted *and* the
   wait was signal-interrupted; other negated errnos only on a setup failure
   before any SQE was consumed.

`flags` reserved (0) in 3G; room for `SLOPRING_ENTER_GETEVENTS` /
`SLOPRING_ENTER_SQ_WAIT` later. The waiter in step 3 **is the user task itself**
— there is no kernel-side future, helper thread, or `(ring, op)` wake callback
(§ 9 explains why the latter cannot exist on this substrate). This is the whole
"sync kernel" claim: `ring_enter`'s block is the *same* multi-fd poll/select
wait (`core/src/syscall/fs/poll_ioctl_handlers.rs`) that `poll(2)`/`select(2)`
already use — register on N deduped queues (`subscribe_current`),
`block_current_task_with_timeout(deadline)`, re-probe on wake, post-block signal
check, unregister.

### 6.3 Per-ring serialization (kernel-side mutual exclusion)

The SPSC discipline of § 4.2 governs the *shared-memory* indices, but the OS
cannot assume userspace honours single-threaded access: a multithreaded process
(or a `dup`'d ring fd) can call `ring_enter` on one ring from two threads at
once. Both would mutate the kernel-side `sq_head` / `cq_tail` and the in-flight
`HandleTable` concurrently → double-consumed SQEs, garbled CQE posts, table
corruption. The kernel therefore holds a **per-ring `SpinLock`** (the OSTD lock
primitive, `slopos-ostd/src/sync/spin.rs`; there is no `IrqMutex` type in the
tree) stored in the ring object, across the whole submit+complete bookkeeping of
`ring_enter`. This is what Linux's `uring_lock` does. The complete-phase block
releases the underlying scheduler's runqueue locks as usual
(`block_current_task_with_timeout` parks without holding spinlocks across the
yield); the per-ring lock is dropped before the task actually sleeps so a second
harvesting thread is not
locked out — 3G must order this carefully (the lock guards the *bookkeeping
mutations*, not the sleep). The simplest correct shape for 3G: take the lock,
run submit, post inline CQEs, build the deduped in-flight `KernelEvent` set,
then drop the lock and run the poll/block on a snapshot, re-taking the lock only
to post deferred CQEs and advance indices.

---

## 7. Submission Model

Per `ring_enter`, the kernel runs this straight-line loop (pseudocode; real code
is sync Rust in `ring/`):

```
sq_tail = uframe.load_u32_acquire(SQ_TAIL_OFF)   // user-written, snapshot once
n = min(to_submit, sq_entries, sq_tail.wrapping_sub(ring.sq_head))  // CLAMP all three
for _ in 0..n {
    idx = ring.sq_head & sq_mask
    sqe = uframe.copy_out_volatile::<Sqe>(sqe_off(idx))   // private kernel copy (§ 5.3)
    ring.sq_head = ring.sq_head.wrapping_add(1)
    process_sqe(ring, &sqe)                        // validate + dispatch (§ 12)
}
uframe.store_u32_release(SQ_HEAD_OFF, ring.sq_head)   // publish consumption to user
n_submitted = n
```

The `sq_entries` term in the clamp is load-bearing (§ 13.6): without it a user
that lies `sq_tail = sq_head + 0xFFFF_FFFF` with `to_submit = u32::MAX` would
spin the loop billions of times over the same `& mask` slots — an
un-preemptible hang. `process_sqe` validates the opcode and its fields (§ 13),
then calls the opcode's sync entry point (§ 12). Two outcomes:

- **Inline completion** — the op is ready now, or fails a non-blocking check
  (`-EFAULT`/`-EINVAL`). Post the CQE immediately (§ 8.1). Most `OP_NOP`, ready
  reads/writes, and all validation failures take this path. **Exception:**
  ownership-transferring ops (`OP_ACCEPT`, and any read that consumes kernel
  buffer bytes) must reserve a CQE slot *before* running the side effect — see
  § 11 (a dropped accept-CQE would orphan an fd).
- **Recorded in-flight (would block)** — the non-blocking probe returns
  `-EAGAIN` (data not ready: read on empty pipe, accept on empty queue,
  `OP_POLL_ADD` not yet ready, `OP_TIMEOUT`). The submit phase **records an
  in-flight row only** (§ 9) and moves on. It does *not* register on any
  `EventBus` queue yet, and the submitter never blocks. Registration + CQE
  posting happen in the complete phase (§ 8.2), if and when the caller asks to
  harvest with `min_complete > 0`.

The *submitter never blocks in the submit phase* — that is what lets one user
thread queue many ops. It only blocks in the complete phase, and only if it
asked to (`min_complete > 0`).

### 7.1 The "would block" decision without async — and why the wake target is the *caller*

The substrate offers exactly **one** wait/wake primitive: `subscribe_current`
(`enqueue_current`, `event_bus.rs:105`) enqueues the **currently-running task**,
and a wake (`WaitQueue::wake_one`/`wake_all` → `unblock_task`) does exactly one
thing — flip *that task* to Ready. **There is no callback, no `(ring, op)` wake
target, no re-probe-closure on the wake path.** An earlier draft of this section
proposed "arm a wait whose wake target = (ring, op-handle), post the CQE from the
producer's wake path"; **that primitive does not exist** and building it would be
a *new* OSTD mechanism running inside the producer's held locks (a lock-order
hazard) and, worse, would install fds into a process from *another* process's
context (the `OP_ACCEPT`/SCM_RIGHTS hazard). It is rejected.

The realizable model is the one the substrate already supports — the multi-fd
`poll`/`select` loop in `core/src/syscall/fs/poll_ioctl_handlers.rs` (helpers in
`fs/src/fileio/poll.rs`):

1. **Non-blocking probe at submit (§ 7).** Each blocking syscall path consults a
   *stored* non-blocking flag on the resource (e.g. `slot.nonblocking` /
   `is_nonblocking()`), **not** a per-call argument (verified: `unix_recv`,
   `socket_recvfrom` et al. read stored state, then block via
   `BUS.subscribe(...).wait_event(...)`). So the opcode handler cannot "call it
   non-blocking" by passing a flag. 3G has two equivalent options, recorded so it
   doesn't re-litigate: **(i)** temporarily set the resource's stored
   `nonblocking = true` across the probe and restore it (cheap, but mutates
   shared socket state under the socket's own lock), or **(ii)** call the
   lower-level readiness/try-op primitive the blocking path itself calls before
   it parks (preferred where one exists, e.g. the `Err(nonblocking)` /
   `Err(true) => -EAGAIN` inner result `unix_recv` already computes). Either way
   the *probe* must not reach the path's own `wait_event` park. On data → inline
   completion; on `-EAGAIN` → record in-flight row (§ 9).
2. **Register-then-recheck at harvest (§ 8.3), caller is the waiter.** When the
   caller asks to block (`min_complete > 0`), the complete phase is the multi-fd
   poll/select **loop** (not a single-queue `wait_event`, which binds one queue
   and cannot span N): (a) register the **calling task** on every *distinct*
   in-flight `KernelEvent` queue via `subscribe_current` (dedup the set —
   `enqueue_current`/`remove_current` push/pop one node per call, so registering
   one queue twice leaks a node), recording the registrations via
   `file_poll_track_registrations` for kill-safety; (b) re-probe every in-flight
   op and post ready CQEs; (c) if still `< min_complete`, block via
   `block_current_task_with_timeout(deadline)` (the `sched`/`driver_runtime`
   primitive the real poll loop uses, with a capped re-poll sleep); (d) on wake,
   `remove_current` from every queue + `file_poll_clear_registrations`, then
   check `has_pending_signal()` and re-evaluate. Loop until
   `available_cqes() >= min_complete`, a signal, or the deadline.

**Lost-wakeup freedom (the invariant 3G must preserve).** The register happens
*before* the re-probe (step 2 order is mandatory): a producer that publishes
between a would-block op's submit-phase probe and the harvest-phase block has
already enqueued the caller's wait node, so the post-register re-probe observes
the readiness and either completes or is woken. This is exactly the
register-before-recheck property `event_bus.rs:98-103` documents and the
`poll`/`select` loop relies on. **Violating the order (recheck then register)
reopens the classic lost-wakeup window.**

> **Consequence — deferred completions only progress inside a blocking
> `ring_enter` (§ 8.3).** Because the only wait target is the caller, a
> would-block op's CQE is posted *only* while some thread is parked in
> `ring_enter(min_complete>0)` re-probing. A userspace runtime that submits a
> blocking op and then *pure-polls* the CQ (never calling
> `ring_enter(min_complete>0)`) will never see that completion. This is a real,
> documented narrowing versus Linux io_uring (whose async backend completes
> independently of `io_uring_enter`); it is the price of "no new kernel
> mechanism." 3H's runtime MUST make progress via blocking `ring_enter`, not
> pure poll, for any ring carrying blocking ops. Inline completions (ready ops)
> remain pure-poll-visible.

---

## 8. Completion Model

### 8.1 Inline completion (op didn't block)

The handler builds a `Cqe { user_data, res, flags }` (where `res` is the sync
path's return value verbatim) and byte-copies it into the CQ:

```
cq_head = uframe.load_u32_acquire(CQ_HEAD_OFF)      // user-written
if ring.cq_tail.wrapping_sub(cq_head) == cq_entries {   // CQ full
    increment shared.cq_overflow              // § 11
    set CQ_OVERFLOW feature bit               // — but NEVER for an op whose side
                                              //   effect already ran (§ 11): those
                                              //   reserve a slot first.
} else {
    idx = ring.cq_tail & cq_mask
    uframe.copy_in_volatile(cqe_off(idx), cqe)         // private → shared (§ 5.3)
    ring.cq_tail = ring.cq_tail.wrapping_add(1)
    uframe.store_u32_release(CQ_TAIL_OFF, ring.cq_tail)
}
```

Waking a parked `ring_enter(min_complete>0)` harvester is **not** a separate
completion-queue wake — the harvester is already registered on the underlying
resource queue(s) (`subscribe_current`) and re-probes after each
`block_current_task_with_timeout` wake (§ 7.1, § 8.3). The producer's existing
`BUS.publish(ev)` is what wakes it; no ring-side wake primitive is invented.

### 8.2 Deferred completion (op blocked, then became ready)

A would-block op's CQE is posted **inside the harvesting `ring_enter`**, not from
the producer's wake path (which cannot reach ring state — § 7.1). Sequence: the
parked harvester is woken by the producer's `BUS.publish(ev)` on the resource
queue it registered on; on wake its re-probe step (the loop body after
`block_current_task_with_timeout`) re-runs the non-blocking probe for each
in-flight op; an op that now returns data has its in-flight row removed (§ 9) and
a CQE posted via the § 8.1 sequence; the loop then re-evaluates
`available_cqes() >= min_complete`. The `res` is the sync path's return value
(now non-`EAGAIN`). All of this runs in the *caller's* task context — which is
why `OP_ACCEPT`/SCM_RIGHTS fd installation lands in the caller's own fd table,
not a producer's (§ 12).

### 8.3 Harvesting completions

- **Poll (inline completions only)**: read `cq_tail` (`load_u32_acquire`),
  compare to local `cq_head`; for each available CQE, copy it out and bump
  `cq_head` (`store_u32_release`). Zero syscalls. **Caveat (§ 7.1):** pure poll
  observes *inline* completions only; deferred (was-blocking) completions are
  posted only during a blocking `ring_enter`, so a runtime with blocking ops in
  flight must use the blocking form below to make progress.
- **Block (drives deferred completions)**: call `ring_enter(min_complete=M)`.
  The kernel runs the multi-fd poll/select loop (§ 7.1): register the caller on
  every distinct in-flight resource queue (`subscribe_current`), re-probe, and —
  if still short — `block_current_task_with_timeout(deadline)`, re-probing on each
  wake until `available_cqes() >= M`, a signal, or the deadline.
  `available_cqes()` is defined exactly as Linux: `cq_tail.wrapping_sub(cq_head)`
  — total *unharvested* CQEs (inline + deferred), no per-call baseline. A
  post-block `has_pending_signal()` check returns on `kill()` (with `n_submitted`
  if any, else `-EINTR`; § 6.2). This is the only place `ring_enter` blocks, and
  it blocks the user task exactly like `poll(2)`.

---

## 9. Blocking and the In-Flight Table

The in-flight table is the kernel-side record of armed-but-not-yet-completed
SQEs. It is **plain data** — the antithesis of a suspended future.

- Stored in the per-ring kernel object (the `HandleTable` row, 3G.3), itself a
  `HandleTable<InFlight>` so each in-flight op gets a generation-counter handle
  (an *op token*) usable by `OP_CANCEL` (§ 10) without ABA hazards.
- Each `InFlight` row records: the originating `user_data`, the opcode, the
  resolved kernel target (fd/socket slot/pipe slot), the `KernelEvent` the op
  will register on **at harvest time** (not at submit — § 7.1), and the snapshot
  of validated arguments needed to re-probe (the user buffer's VA+len, re-wrapped
  in a fresh `UserSlice::try_new` on each probe, never a stored reference). It
  holds **no Rust reference** into ring memory and **no suspended stack** — only
  the data to re-run the non-blocking probe.
- Capacity is bounded by `cq_entries` (an op in flight will eventually need a CQE
  slot); attempting to record more than capacity completes the SQE inline with
  `-EAGAIN` (backpressure, § 11), never grows unboundedly.
- **Registration is caller-scoped and deduped.** When a harvesting `ring_enter`
  registers interest, it enqueues the *caller task* once per *distinct*
  `KernelEvent` across all in-flight rows (several ops on one socket share one
  registration). `enqueue_current`/`remove_current` push/pop one node per call,
  so double-registering one queue would leak a node and miscount waiters (§ 7.1).
  3G dedups the event set before registering and unregisters symmetrically.

The lifecycle of one blocking op (caller-as-waiter model, § 7.1):

```
submit (ring_enter A):   probe non-blocking → EAGAIN
                         insert InFlight row (gets op-handle H); register nothing
harvest (ring_enter B,   build deduped KernelEvent set over all in-flight rows
  min_complete>0,        subscribe_current(ev) for each  [register the CALLER task]
  may == A):             reprobe_all(); for each ready op: remove row, post CQE
                         loop: if available_cqes()<M { block_current_task_with_timeout(dl);
                                  reprobe_all(); if has_pending_signal() break }
ready:                   producer BUS.publish(ev)  [existing wake path, unchanged]
                         → wakes the caller task → reprobe posts the CQE
unregister:              on wake/return, remove_current(ev) for each registered ev
                         + file_poll_clear_registrations(task)
cancel (OP_CANCEL):      walk table for H/user_data; remove row; post CQE(-ECANCELED)
```

Because the only durable identifiers are generation-counter handles
(`ring_handle`, `op_handle` H), a cancel or ring-close that races a harvest
resolves a stale handle to a typed "gone" and drops it, never a dangling deref
(AD-11). The wait *node* is owned by the caller task on the resource queue (the
existing poll/select node), so its teardown is the already-audited
`remove_current` path — not a new ring-owned wake object. This is the same
staleness-safety Phase 2H bought for fds and pipes, reused.

---

## 10. Cancellation

Opcode `OP_CANCEL` (§ 12). The kernel side is straight-line sync, with **no
async-cancellation hazard** because there is no suspended computation to cancel —
only an interest registration and a table row to remove.

- `Sqe.addr` carries the `user_data` (or op-handle) of the target in-flight op;
  `op_flags` may carry `SLOPRING_ASYNC_CANCEL_ALL` to cancel every op matching a
  fd.
- Handler walks the in-flight table for the match. For each:
  1. Remove the `InFlight` row. (No `EventBus` unregister is needed in the common
     case: under the caller-as-waiter model (§ 7.1) a would-block op holds *no*
     standing registration between `ring_enter` calls — registration exists only
     for the duration of a harvesting block. If the cancel races a concurrent
     harvest that has the caller registered, the next `reprobe_all()` simply
     skips the now-removed row; the caller's own `remove_current` on wake tears
     down its node. There is no per-op registration to leak.)
  2. Post a CQE for the cancelled op with `res = -ECANCELED`.
  3. Post a CQE for the `OP_CANCEL` SQE itself with `res = 0` (found) or
     `-ENOENT` (no match) / `-EALREADY` (completion already posted).
- If the target op has *already completed* (its CQE is posted/pending), cancel is
  a no-op returning `-EALREADY` — the completion stands. This matches Linux
  io_uring `IORING_OP_ASYNC_CANCEL` semantics.

**Where async cancellation actually belongs.** A stuck *future* is a userspace
concern: dropping a future in the 3H runtime submits an `OP_CANCEL` for its
in-flight op (3H.4). A stuck op degrades exactly one process, never the kernel —
precisely the property the retired async-first design could not guarantee
(the retired async-first design's unsolved problem: "cancellation safety is unsolved as a default
property in any language"). SlopRing sidesteps it: the kernel's cancel is
deleting a table row, which is always safe.

---

## 11. Backpressure and Overflow

Two independent flow-control surfaces, both matching Linux semantics so the
mental model transfers:

- **SQ full (`sq_tail - sq_head == sq_entries`)**: userspace cannot enqueue more
  SQEs. It must `ring_enter` to let the kernel drain (advance `sq_head`), or wait
  for in-flight completions to free CQ space first. This is a pure userspace
  spin/retry; the kernel never sees an "SQ full" — it only ever reads as many
  SQEs as `sq_tail` exposes.
- **CQ full — the ownership rule (load-bearing).** A CQE can carry *ownership*
  the op already produced or consumed: `OP_ACCEPT`'s `res` is a freshly installed
  fd, and a completed `OP_READ`/`OP_RECVMSG` has already *consumed* bytes from
  the kernel buffer. **Dropping such a CQE leaks the fd+socket (orphaned in the
  process fd table, un-`close`able) or silently destroys consumed data** — a
  userspace-triggerable kernel-resource exhaustion, not merely "a missed result."
  Therefore the rule:
  - **Side-effecting ops reserve a CQE slot *before* running the side effect.**
    `OP_ACCEPT` and any consuming read check `cq_tail.wrapping_sub(cq_head) <
    cq_entries` (a slot is free) *before* dispatching; if the CQ is full they do
    **not** run — they complete inline with `-EAGAIN` (backpressure) and leave
    the resource untouched. The op is retried by userspace after it drains the
    CQ. **No accepted fd or consumed byte is ever dropped.**
  - **Side-effect-free results may drop with a counter.** Only completions whose
    loss costs nothing — `OP_NOP`, `OP_POLL_ADD` readiness, `OP_TIMEOUT`
    `-ETIME`, and errno-only failures — use the Linux-style drop-on-overflow:
    increment the shared `cq_overflow` counter, set the `SLOPRING_CQ_OVERFLOW`
    flag (mirrors `IORING_SQ_CQ_OVERFLOW`), discard. Userspace observing the flag
    knows it missed `cq_overflow` readiness/timeout notifications and must drain
    faster.
  Because `cq_entries = 2 * sq_entries` and the in-flight table is capped at
  `cq_entries`, the reserve-before-side-effect check rarely fires in practice; it
  is the *safety net* that makes the "surfaced honestly" claim true for ownership
  ops. (A future `IORING_FEAT_NODROP`-style backlog list — which would let even
  ownership ops complete without an immediate slot by parking the CQE in a kernel
  backlog — is a post-Phase-3 candidate; 3G ships reserve-before-side-effect +
  drop-with-counter-for-the-rest, which is bounded and trivially sync.)
- **In-flight table full**: an SQE that would record an in-flight row but finds
  the table at capacity completes inline with `-EAGAIN`, so userspace throttles
  naturally.

Backpressure never blocks the kernel and never allocates unboundedly — both
queues and the in-flight table are fixed-capacity, sized at `ring_setup`.

---

## 12. Opcode Catalogue and Sync-Path Justification (3F.2)

**The parity contract.** Each opcode is a thin adapter: validate SQE fields →
run the *existing* sync syscall path's **non-blocking probe** → on data, post
CQE with that return value; on `-EAGAIN`, record an in-flight row and let the
harvest phase register the caller and re-probe (§ 7.1, § 9). The opcode
introduces **no new blocking primitive** — it reuses the exact code path the
equivalent sync syscall uses, which is *why* observable results match (R12).
3G.6 pins this with a parity test per opcode that drives the same input through
both the opcode and the sync syscall and diffs the result.

**Two cross-cutting realities 3G must handle (verified against source).**
1. **There is no per-call non-blocking argument.** Every cited entry point
   (`unix_recv`, `socket_recvfrom`, `socket_send`, `unix_accept`, …) reads the
   resource's *stored* `nonblocking` flag (`slot.nonblocking` /
   `Socket::is_nonblocking()`), then parks via `BUS.subscribe(...).wait_event`.
   So the handler cannot "call it non-blocking" by passing a flag; it either
   (i) toggles the resource's stored flag across the probe and restores it, or
   (ii) calls the inner try-op the path computes before parking (the
   `Err(true) => -EAGAIN` result `unix_recv` already produces). 3G picks per
   family; the probe must never reach the `wait_event` park (§ 7.1).
2. **AF_INET and AF_UNIX entry points have different ABIs.** AF_INET
   (`socket_recvfrom`/`socket_send`/`socket_accept`) take **raw user pointers**
   (`*mut/*const u8`) and copy against the *current* process address space;
   AF_UNIX (`unix_recv`/`unix_send`/`unix_recvmsg`/`unix_accept`) take **kernel
   slices** (`&[u8]`) and return `Result<_, i32>`. The ring handler resolves the
   fd's `FileOps`, dispatches via `is_unix_socket()`, and **marshals per family**
   — for AF_UNIX it stages user↔kernel through a validated `UserSlice` itself;
   for AF_INET it forwards the validated user VA. "Same function" is true; the
   argument marshalling is not uniform.

Nine opcodes (framekernel subtasks 3G.4.{a..i}):

| Opcode | Sync entry point reused | SQE fields | Blocks on (`KernelEvent`) | CQE `res` |
|---|---|---|---|---|
| **`OP_NOP`** | none (benchmark/no-op) | — | never | `0` |
| **`OP_READ`** | `file_read_fd(pid, fd, &mut IoBufWrite)` (`fs/src/fileio/fdops.rs`) | `fd`, `addr`/`len` (user dst buf), `off` | `PipeRead` (pipe), `SocketRecv`/`UnixSocket` (socket fd), `TtyInput` (tty); regular files never block | bytes read, or `-errno` |
| **`OP_WRITE`** | `file_write_fd(pid, fd, &IoBufRead)` (`fs/src/fileio/fdops.rs`) | `fd`, `addr`/`len` (user src buf), `off` | `PipeWrite` / `SocketSend` / `TtyOutput` (when buffer full) | bytes written, or `-errno` |
| **`OP_RECVMSG`** | `unix_recvmsg` (`net/src/unix_socket/mod.rs`) / `socket_recvfrom` (`net/src/socket.rs`) | `fd`, `addr` (user `MsgHdr*`), `op_flags` | `UnixSocket` / `SocketRecv` | bytes received, or `-errno` |
| **`OP_SEND`** | `unix_send`/`unix_sendmsg` / `socket_send`/`socket_sendto` | `fd`, `addr`/`len` (or `MsgHdr*`), `op_flags` | `UnixSocket` / `SocketSend` | bytes sent, or `-errno` |
| **`OP_ACCEPT`** | `unix_accept` / `socket_accept` | `fd` (listener), `addr` (out `sockaddr*`), `addr2` (out `socklen*`) | `UnixSocket` / `SocketAccept` | new fd, or `-errno` |
| **`OP_POLL_ADD`** | `file_poll_register_fd` to register + `file_poll_fused`/per-fd readiness probe to read (`fs/src/fileio/poll.rs`) | `fd`, `op_flags` (poll mask, e.g. `POLLIN`) | the fd's readiness queue (`PipeRead`/`SocketRecv`/`TtyInput`/`UnixSocket`) | readiness bitmask, or `-errno` |
| **`OP_TIMEOUT`** | the harvest-block deadline (`block_current_task_with_timeout`, `sched`) | `off` (timeout, ns) | no `KernelEvent`; sets the harvest block's deadline (note below) | `-ETIME` on expiry, `0` if cancelled-by-completion |
| **`OP_CANCEL`** | in-flight table walk (§ 10) | `addr` (target `user_data`/op-handle), `op_flags` | never (synchronous walk) | `0` / `-ENOENT` / `-EALREADY` |

Notes on parity-preserving details:

- **`OP_READ`/`OP_WRITE`** wrap the user buffer in the existing `IoBufWrite` /
  `IoBufRead` *traits* (`abi/src/io.rs`) via the `UserReadBuf`/`UserWriteBuf`
  adapters (`mm/src/user_io_buf.rs`, wrapping a validated user VA+len) — the
  *same* adapters `fs_read`/`fs_write` build, so the copy semantics
  (`copy_out`/`copy_in`, `IO_STAGING_SIZE` staging) are byte-identical. (The IO
  adapters wrap a raw VA+len, not a `UserSlice` — there is no `UserSlice`-based
  IO buffer in the tree; validation is the adapter's own bounds check plus the
  per-op `UserSlice::try_new` of § 13.4.)
- **Socket opcodes** route AF_INET vs AF_UNIX via `FileOps::is_unix_socket()`
  (`abi/src/file_ops.rs`), then marshal per family (cross-cutting reality 2
  above): `unix_accept` returns `Result<SocketHandle, i32>` while `socket_accept`
  returns `i32` — the handler reconciles both into the CQE `res` (new fd or
  `-errno`).
- **`OP_POLL_ADD`** result is readiness, not bytes. `file_poll_register_fd`
  returns a *registration handle* (`PollRegInfo`), **not** a readiness mask — so
  the handler registers with it and reads readiness via the fused probe
  (`file_poll_fused` → `FusedPollResult`), reporting the same bits `poll(2)`
  would. (Earlier drafts mis-stated `file_poll_register_fd` as returning
  readiness; corrected.)
- **`OP_TIMEOUT` is the *harvest-wait deadline*, not an independently-firing
  op.** A timed block fires by unblocking the *parked task*
  (`block_current_task_with_timeout` is exactly the timed-block primitive the
  poll/select loop uses) — there is no "arm a timer that posts a CQE while nobody
  is parked." A standalone `OP_TIMEOUT` submitted with `min_complete=0` and
  pure-polled can never fire. Its realizable meaning: the in-flight `OP_TIMEOUT`
  row sets the minimum deadline passed to the *next blocking* `ring_enter`'s
  `block_current_task_with_timeout`; on expiry the harvest posts `-ETIME` for it.
  (`syscall_sleep_ms` uses `sleep_current_task_ms`, a *different* path — not the
  OP_TIMEOUT model.)
- **`connect`/`bind`/`listen` are out of the nine-opcode set.** The 3H.3 `nc`
  port is therefore data-plane only: its `recv`/`send`(/`accept`) loop is
  ring-driven, while connect/bind/listen stay regular blocking syscalls. Stated
  so 3H doesn't discover mid-port that it can't issue `connect` over the ring; an
  `OP_CONNECT` is a clean post-Phase-3 addition.
- **No opcode adds a syscall number.** All nine ride inside `ring_enter`. The
  underlying sync syscalls stay exactly as they are — which is the point:
  Verus's target (the OSTD critical path) and the syscall surface stay frozen
  while the async edge is added on top.

---

## 13. Threat Model (3F.3)

> **Premise.** Every byte userspace can reach is hostile. SQEs are
> user-controlled bytes in shared, concurrently-mutable memory. The kernel side
> must be sound under a userspace that writes garbage SQEs, mutates them
> mid-flight, races multiple threads on the ring, points buffers at unmapped or
> kernel memory, and replays stale ring fds. None of these may produce UB,
> privilege escalation, or kernel memory disclosure — at worst a typed error
> CQE or `EFAULT`/`EINVAL`/`EBADF`.

### 13.1 Asset / boundary inventory

| Asset | Exposure | Defence |
|---|---|---|
| Ring shared frames | mapped read+write into user | `UFrame<RingMeta>` (Inv. 4/5); **volatile/atomic** byte-copy only (§ 5.3, the new OSTD accessor of § 3); never a kernel `&T` |
| `Sqe` contents | fully user-controlled, mutable any time | snapshot-then-validate (§ 13.3); read once via `copy_out_volatile`, validate *the snapshot*, act only on it |
| Ring frame vs. user mapping | frame freed on ring-object drop; user PTEs may outlive the fd | ring VMA holds a `Frame<RingMeta>` ref so the frame can't be freed while mapped (§ 14, no mmap-after-close UAF) |
| User buffers (`addr`/`len`) | user VAs in the SQE | re-validated through `UserSlice::try_new` (range, overflow, user-half) before every access (§ 13.4) |
| `ring_fd` | user-supplied integer | fd table + `HandleTable` generation check → `-EBADF` on stale/foreign (§ 13.5) |
| In-flight op token | user-referenced by `OP_CANCEL` | generation-counter `Handle`; stale → `-ENOENT`, never deref |
| `sq_tail` / `cq_head` indices | user-written | clamped to `sq_entries`, never trusted; wrapping arithmetic; `(tail-head) <= entries` re-derived (§ 13.6) |
| Kernel `sq_head` / `cq_tail` | kernel-owned | mirrored into shared page on write; kernel reads its *own* copy, not the shared one, for control decisions |

### 13.2 Why no kernel reference into the ring (restating the central rule)

A `&Sqe` into ring memory is UB the instant userspace's other thread writes that
SQE, because `&T` is a no-mutation promise the hardware can break. This is the
exact Theseus-inherited bug class AD-3 closes. SlopRing forbids it structurally:
ring frames are `AnyUFrameMeta`, whose contract (verified for `VmSpace` in 3D)
is "byte-copy interface only, never `&T`/`&mut T`". The kernel reads SQEs by
copying 64 bytes into a private `Sqe` on its stack and writes CQEs by copying 16
bytes out of a private `Cqe`. There is no point at which a live Rust reference
aliases user-writable memory.

### 13.3 TOCTOU on SQE fields — and the data-race underneath it

Two distinct hazards, both closed:

- **Value-level TOCTOU.** Because userspace can mutate an SQE between the kernel
  reading field A and field B, the kernel **reads each SQE exactly once, into a
  private snapshot**, then validates and acts on the snapshot only (§ 5.3, § 7).
  A mid-flight mutation changes the *next* read of that slot, never the in-flight
  op. No field is re-read from shared memory after validation. This collapses the
  TOCTOU class to "userspace raced its own submission," which is harmless — it
  gets a result consistent with *some* serialization of its writes.
- **Access-level data race.** The snapshot *read itself* races the concurrent
  user writer. A plain non-atomic `read_pod`/`read_bytes` racing a write is UB in
  the Rust/LLVM abstract machine *regardless* of the value-level argument above
  (and emits no fence). The snapshot is therefore taken with the **volatile**
  `copy_out_volatile` accessor and indices with `load_u32_acquire` (the new OSTD
  surface, § 3) — mirroring Linux's `READ_ONCE`/`smp_load_acquire`. Without this,
  § 13.2's "byte-copy only" would still be a race; *with* it, the only
  kernel↔ring contact is well-defined volatile copies.

### 13.4 User-pointer validation (`addr`, `addr2`, `MsgHdr*`)

Every user VA in a snapshot SQE is validated through the existing
`UserSlice<T>::try_new(addr, count)` / `UserPtr<T>::try_new` path
(`slopos-ostd/src/user/ptr.rs`) before any access: it checks multiplication
overflow, that the range lies wholly in the user half (`USER_SPACE_START_VA ..
USER_SPACE_END_VA`), and produces a typed handle whose only operations are
checked byte copies. A buffer pointing at unmapped memory faults *in the copy*
and surfaces as `-EFAULT` in the CQE — never a kernel page fault that escalates.
The opcode handlers reuse the *same* validation the equivalent sync syscalls
use, so there is no second, weaker validation path.

### 13.5 Stale / forged ring fd (AD-11)

`ring_enter(ring_fd, …)` resolves `ring_fd` through the process fd table to a
`FileKind::Ring` open file, then through the per-process ring `HandleTable` to
the ring object. Both lookups are generation-checked: a closed-then-reused fd
number, a fd belonging to another `FileKind`, or a ring handle whose generation
no longer matches all resolve to typed errors (`-EBADF` / `HandleError`), never a
dangling pointer. This is the Phase 2H staleness-safety property reused verbatim.

### 13.6 Index manipulation and integer safety

The kernel treats `sq_tail` / `cq_head` as adversarial: it loads each once,
computes available counts with wrapping `u32` arithmetic, and **clamps** the
work it does — critically including the `sq_entries` bound:
`n = min(to_submit, sq_entries, sq_tail.wrapping_sub(sq_head))` (§ 7). The
`sq_entries` term is not optional: without it a user lying
`sq_tail = sq_head + 0xFFFF_FFFF` with `to_submit = u32::MAX` drives a
multi-billion-iteration loop over the same `& mask` slots — an un-preemptible
hang, not merely slow (Phase 3 keeps the preemptive path live, but an in-syscall
spin still starves the CPU). With the clamp, a lying `sq_tail` at most makes the
kernel re-read SQE slots it owns the indices for, all in-bounds of the fixed
array. A user that never advances `cq_head` triggers CQ overflow (§ 11) — and for
ownership ops, reserve-before-side-effect makes that a self-inflicted `-EAGAIN`
throttle, never a leaked fd (§ 11). No arithmetic feeds an unchecked index or
allocation size.

### 13.7 Reference lifetime: nothing outlives one `ring_enter` (3F.3 core claim)

**No kernel reference into ring memory outlives a single `ring_enter`
invocation.** Within one `ring_enter`, the kernel forms only transient byte-copy
accesses (snapshot in, CQE out) that complete before the call returns. The
*in-flight table* persists across calls, but it holds **no reference into the
ring** — only owned, copied data (the validated args needed to re-probe, re-wrapped
in a fresh `UserSlice::try_new` each time). When a deferred completion posts
(§ 8.2), that CQE write is itself a fresh transient volatile byte-copy inside the
harvesting `ring_enter` (the only context that posts deferred CQEs, § 7.1). So at
no time between `ring_enter` calls does the kernel hold a borrow into user-shared
memory. This is what makes the soundness argument local and total: the only
kernel↔ring interactions are bounded volatile byte-copies, each fully contained
in one sync call.

### 13.8 Resource exhaustion

- **Memory**: `ring_setup` allocations are bounded by `SLOPRING_MAX_ENTRIES`
  (proposed 4096) and counted against the process; a process spamming
  `ring_setup` is bounded by the fd limit (each ring is an fd).
- **In-flight**: capped at `cq_entries` per ring (§ 9); excess rows → `-EAGAIN`.
- **CQ**: fixed capacity. Side-effect-free completions drop with a counter
  (§ 11); **ownership completions (`OP_ACCEPT`, consuming reads) reserve a slot
  before the side effect and `-EAGAIN` if none is free**, so a full CQ can never
  leak an accepted fd/socket or destroy consumed bytes — the resource cap is the
  honest bound, not just the CQE count (§ 11).
- **Kernel CPU**: each SQE is O(1) validate + one sync-path probe; a batch is
  O(K) with `K = min(to_submit, sq_entries, sq_tail - sq_head)` — the
  `sq_entries` clamp (§ 7, § 13.6) is what makes this bounded. No unbounded loop.

### 13.9 No `async fn`, ever (R13)

The threat that `ring/` accretes "just one async helper" is closed by
`scripts/check_no_kernel_async.sh` (3A.5), which fails the build on any `async
fn` in any kernel crate, `ring/` included. This is a *build-time* guarantee, not
a convention — the same load-bearing-in-CI discipline as
`check_unsafe_outside_ostd.sh`.

---

## 14. Process Lifecycle: fork / exec / exit / signal

Because a ring is an open file (`FileKind::Ring`), it inherits the fd lifecycle
machinery instead of inventing its own:

- **`fork`**: a ring fd is **closed in the child** (close-on-fork), *decided*,
  not floated. Rationale: the SQ/CQ is a single-producer/single-consumer ring
  (§ 4.2); letting both parent and child hold the same ring fd would put two
  producers on one SQ with no cross-process discipline, and the per-ring kernel
  lock (§ 6.3) serializes but does not *partition* the shared indices. Equally,
  intra-process `dup` of a ring fd is allowed (it is what makes the per-ring lock
  of § 6.3 mandatory) but multi-threaded use is then the app's responsibility.
  The alternative — "child fd resolves but must re-map" — is **rejected**: it is a
  second producer on one SQ. The 3G.6 "ring-FD inheritance across fork"
  test asserts the child's ring fd is closed.
- **`exec`**: address space is replaced, so the ring mapping is torn down with
  the old `VmSpace`; the ring object is dropped when its last fd closes (exec
  closes fds per the process model). No special-casing.
- **mmap-after-close UAF (the ordering that must be pinned).** Ring frames are
  mapped into user space by raw paddr (the `process_vm_mmap_shared` precedent
  installs PTEs to the physical address, not via a `Frame` ref held by the VMA).
  The fd lifetime and the VMA lifetime are independent: a process can `close` the
  ring fd while keeping the mapping. If the ring object's drop frees the
  `Frame<RingMeta>` region (via `on_drop`) while user PTEs still point at it
  writable, the frame is recycled into kernel use while userspace can still
  read/write it — a UAF / kernel-memory disclosure. **Fix (mandatory for 3G):**
  the ring VMA holds a `Frame<RingMeta>` reference so the frame cannot be freed
  while mapped (the way a sound `memfd` keeps pages alive across `close`);
  equivalently, the ring object's drop force-unmaps the region from the
  `VmSpace` *before* releasing the frames. "Drops the region" is doing too much
  work in one phrase otherwise.
- **`exit` / process teardown**: closing the last ring fd (and tearing down the
  mapping per the rule above) drops the ring object, which drops the in-flight
  table (plain data — no standing `EventBus` registration exists between
  `ring_enter` calls under the caller-as-waiter model, § 7.1/§ 9, so there is
  nothing to unregister; any task currently parked in `ring_enter` is itself the
  registrant and tears down its own nodes via `remove_current` on wake — the
  same path the Phase-2 "reap blocked-task resources on async kill" commit
  `962b1999` uses), and drops the `Frame<RingMeta>` region (frames returned via
  `on_drop`). No leak, no dangling registration. A `HandleTable` lookup that
  races teardown resolves the ring handle to "gone" and is dropped (§ 9).
- **Signal during `ring_enter(min_complete>0)`**: 3G wires `has_pending_signal()`
  (`core/src/driver_hooks.rs` `runtime_has_pending_signal`, via
  `kernel-services::driver_runtime`) directly into the ring completion-wait
  predicate (§ 8.3) — it is **not** inherited for free, and note the AF_UNIX wait
  path does not currently carry the probe, so the ring's own predicate is the
  signal-delivery point regardless of opcode family. A `kill()` makes the
  predicate true; `ring_enter` returns `n_submitted` if any SQE was consumed,
  else `-EINTR` (§ 6.2 — never discard a submission, or userspace double-submits),
  with all already-posted CQEs valid in the CQ. The 3G.6
  "kill-during-`ring_enter`" test pins both the `-EINTR` and the
  submit-count-preservation.

---

## 15. Verification and Test Strategy

SlopRing carries no Verus obligation of its own (the verified critical path is
OSTD's `Frame`/`slab`/`VmSpace`, already done in 3B–3D; SlopRing *consumes* those
verified primitives). Its assurance comes from:

1. **`#![forbid(unsafe_code)]` on `ring/`** (3G.1) — it is a non-OSTD kernel
   crate, so it cannot contain `unsafe`; all memory access goes through the
   OSTD `UFrame`/`UserSlice`/`cursor` surface, *extended* in 3G with the small
   volatile/atomic accessor of § 3 (which lives in OSTD, under its own audit —
   item 5). `check_unsafe_outside_ostd.sh` enforces the `ring/` boundary.
2. **`check_no_kernel_async.sh`** — no `async fn` in `ring/` (§ 13.9, R13).
3. **Per-opcode parity tests** (3G.6, R12) — each opcode driven through the same
   input as its sync syscall, results diffed. CI-gated; an opcode without a
   parity test is a build failure.
4. **Adversarial tests** (3G.6) — backpressure (SQ/CQ full, overflow counter),
   cancellation (cancel pending / already-complete / nonexistent), stale ring fd
   → `-EBADF`, garbage opcode → `-EINVAL`, out-of-range buffer → `-EFAULT`,
   fork-inheritance behaviour, kill-during-`ring_enter` → `-EINTR`.
5. **KernMiri** over the byte-copy snapshot/post paths and **the new
   `UFrame::{load_u32_acquire, store_u32_release, copy_out_volatile,
   copy_in_volatile}` accessor** (§ 3) — the one piece of new OSTD `unsafe`
   SlopRing introduces, and the part most likely to harbour a latent aliasing or
   ordering bug. It carries its own `// SAFETY:` note (naming Inv. 4/5) and is in
   the KernMiri scope, reusing the Phase-1 harness.
6. **Concurrency tests** (3G.6) — two threads racing `ring_enter` on one ring fd
   (the per-ring lock of § 6.3 must serialize them; assert no double-consumed SQE
   / garbled CQE), and the register-before-recheck lost-wakeup window (§ 7.1):
   readiness arriving between submit-probe and harvest-block must still complete.
7. **End-to-end** (3H.3) — `nc`'s `recv`/`send` loop ported to `Ring`, driven by
   a **blocking `ring_enter`** harvest (not pure poll — § 7.1/§ 8.3), passing its
   existing test fixtures, proves the edge works for a real app.

---

## 16. Open Questions Deferred to 3G/3H

Recorded so implementation doesn't re-litigate settled-enough-to-defer points:

1. **Probe model** (§ 7.1) — **decided**: caller-as-waiter poll over the in-flight
   set; the "(ring, op) wake target" model is rejected (no such substrate
   primitive). Open *latency* sub-question for 3G to benchmark: whether to keep
   in-flight rows registered across enters (would need new mechanism) vs. the
   register-only-while-harvesting model shipped here.
2. **fork inheritance** (§ 14) — **decided**: close-on-fork (SPSC integrity).
3. **`ring_setup` arity** (§ 6.1) — **decided**: honest `(entries, params*)`
   signature; no `flags`-slot pun.
4. **CQ overflow policy** (§ 11) — **decided**: reserve-before-side-effect for
   ownership ops + drop-with-counter for the rest; `NODROP` backlog is
   post-Phase-3.
5. **`slibc-ring` runtime shape** (3H.1/3H.2) — pick at 3H.1: hand-rolled poll
   loop vs. vendored embassy-style executor. Userland, outside kernel discipline.
6. **Multi-shot opcodes** (`POLL_ADD` with `IORING_POLL_ADD_MULTI`,
   `RECV_MULTISHOT`) — out of scope for 3G's nine opcodes; the `Cqe.flags`
   `F_MORE` bit is reserved so they can land later without ABI churn.

---

## 17. Glossary

| Term | Definition |
|---|---|
| **SQ / CQ** | Submission Queue / Completion Queue — the two SPSC ring buffers in shared memory. |
| **SQE / CQE** | Submission / Completion Queue Entry — the 64 B / 16 B wire structs (§ 4.4, § 4.5). |
| **`ring_setup` / `ring_enter`** | The two new sync syscalls (§ 6). |
| **In-flight table** | Kernel-side record of armed-but-incomplete SQEs — plain data, not a future (§ 9). |
| **Caller-as-waiter** | The complete-phase model: a harvesting `ring_enter` registers the *calling task* (not a `(ring, op)` callback) on every in-flight resource queue, re-probes, and blocks — the poll/select shape (§ 7.1). |
| **Inline vs. deferred completion** | CQE posted during the submit phase (op was ready) vs. inside a *blocking* `ring_enter` after the op became ready (§ 8). Deferred completions are not visible to pure-poll harvest (§ 7.1). |
| **`RingMeta`** | New `AnyUFrameMeta` frame-metadata type for ring shared pages (§ 5.2). |
| **`RingFd`** | An fd of `FileKind::Ring` referring to a ring object in the per-process ring `HandleTable` (§ 6.1, § 13.5). |
| **Sync core, async edge** | The architecture: kernel synchronous, async lives in the userspace runtime over the ring (§ 2, AD-8/AD-9). |

---

## 18. References

### io_uring prior art
- Jens Axboe, *Efficient IO with io_uring* — https://kernel.dk/io_uring.pdf (the SQ/CQ + head/tail + overflow model SlopRing trims).
- `liburing` — https://github.com/axboe/liburing (the userland shape `slibc-ring` mirrors, 3H.1).
- Linux `io_uring(7)` / `io_uring_setup(2)` / `io_uring_enter(2)` man pages (semantics SlopRing matches: `EINTR`, `ECANCELED`, CQ overflow).

### SlopOS substrate (grounding this design — all durable source, no plan files)
- Framekernel terminology used here (concept labels, not document paths):
  **AD-8/AD-9** (sync kernel, async edge), **AD-3** (no `&T`/`&mut T` over
  user/MMIO/DMA memory), **AD-11** (generation-counter handles), **Inv. 4/5**
  (sensitive memory untouchable by clients / user programs), **R12** (opcode
  parity), **R13** (no `async fn` in any kernel crate).
- `slopos-ostd/src/sync/event_bus.rs`, `wait_queue.rs` — the blocking substrate (§ 3).
- `slopos-ostd/src/mm/frame.rs`, `uframe.rs`, `vm_space.rs` — `Frame<M>`/`UFrame`/`cursor` (§ 5); the 3B/3D verified primitives.
- `slopos-ostd/src/handle.rs` — generation-counter `Handle`/`HandleTable` (§ 9, § 13.5; AD-11 / Phase 2H).
- `slopos-ostd/src/user/ptr.rs` — `UserSlice`/`UserPtr` validation (§ 13.4).
- `mm/src/process_vm.rs` (`process_vm_mmap_shared`), `mm/src/memfd.rs` — shared-frame mapping precedent (§ 5.1).
- `fs/src/fileio/fdops.rs` (`file_read_fd`/`file_write_fd`), `fs/src/fileio/poll.rs` (`file_poll_register_fd`), `abi/src/file_ops.rs` (`FileOps`/`FileKind`), `abi/src/io.rs` (`IoBufRead`/`IoBufWrite`) — opcode sync entry points (§ 12).
- `net/src/socket.rs`, `net/src/unix_socket/mod.rs` — socket opcode sync entry points + the `has_pending_signal()` wait pattern (§ 12, § 14).
- `core/src/syscall/macros.rs` (`define_syscall!`), `abi/src/syscall/numbers.rs` — syscall definition + number allocation (§ 6).
- Commit `962b1999` — "reap blocked-task resources on async kill" (the teardown SlopRing reuses, § 14).
