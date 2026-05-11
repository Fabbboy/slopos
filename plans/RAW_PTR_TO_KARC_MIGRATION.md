# Migrate `*mut T` to `KArc<T>` for kernel-shared ownership

## Intent

SlopOS should migrate every kernel subsystem that currently shares object ownership via raw `*mut T` + an in-struct refcount (`Task::refcnt` + `task_inc_ref` / `task_dec_ref`, sockets, FDs, etc.) to `KArc<T>` — the kernel's fallible-alloc `Arc` equivalent. This is the Rust-idiomatic shape and aligns the codebase around one ownership primitive instead of two.

## Why

Today the kernel mixes two patterns:

1. **Raw pointer + open-coded refcount on the type itself.** Most of `core/` (scheduler, sockets, fileio, etc.) does this. `Task` carries `pub refcnt: AtomicU32`; lifetime is managed by hand via `task_inc_ref` / `task_dec_ref`.
2. **`KArc<T>`** for a handful of subsystems (`appkit`, some drivers).

The mixed approach is the source of recurring lifetime bugs and confused review:
- Every `*mut Task` callsite has to be audited for "did the caller bump refcnt before passing this pointer?"
- Refcount underflow is undefined and only surfaces as use-after-free much later.
- `Send` / `Sync` are asserted by hand on raw-pointer wrappers (`SendTaskHandle` and friends) instead of being derived from `KArc<T>`'s guarantees.
- Type signatures lie: `*mut Task` carries no lifetime, but the contract demands a held refcount.

`KArc<T>` collapses this to a single primitive whose semantics the type system enforces. Cloning is the refcount bump; drop is the decrement; aliasing is the borrow checker's problem.

## Scope (rip and replace, no shims)

- `core::scheduler::task_struct::Task::refcnt` and its `inc_ref` / `dec_ref` / `ref_count` methods — deleted.
- All `*mut Task` parameters across the scheduler, IPC, fileio, signals, futex — replaced by `KArc<Task>` (or `&Task` where short-lived).
- `Task::next_ready` / `Task::next_inbox` stay as intrusive link / Treiber-stack slots; intrusive lists hold a non-owning pointer derived from a `KArc<Task>` whose owning reference lives elsewhere (the runqueue's separate `KArc` table, the sleeper map, etc.). The two paradigms coexist: `KArc` for ownership, intrusive linkage for hot-path O(1) queueing. Linux does the equivalent (`struct task_struct *` + `get_task_struct`/`put_task_struct` + intrusive lists).
- Same migration in `net::socket`, `fs::fileio`, anywhere else a struct carries its own refcount.

## Out of scope

- The scheduler's intrusive lists (`ReadyQueue`, `ZombieList`, `WaitQueue`) — those stay intrusive for per-op cost reasons. Intrusive linkage and `KArc` ownership are orthogonal; this migration keeps both.
- Anything outside the kernel allocation discipline (`userland/`, `slibc/`, `slop-protocol/`, etc. — they already use `alloc::*`).

## Acceptance

- No `pub refcnt: AtomicU32` fields remain on kernel types.
- No `*mut Task` (or analogous) appears in function signatures except inside the intrusive-list / Treiber-stack primitives that genuinely require raw pointers.
- A `grep` for `inc_ref` / `dec_ref` / `task_inc_ref` / `task_dec_ref` returns zero hits.
- Every `unsafe impl Send` / `unsafe impl Sync` for a raw-pointer wrapper is either deleted or replaced by a `KArc<T>`-based equivalent whose `Send` / `Sync` derives automatically.

## Status

Not yet started. This document records the intent so the migration can be scheduled as a coherent rip-and-replace rather than a drift across many incremental PRs.
