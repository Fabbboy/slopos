# Safe-by-design: turning the bandaids into type-level invariants

## Background

This branch (`sched/fix-self-wakeup-deadlock`) landed six point fixes
across the scheduler, page allocator, and kernel heap. Each chases a
single shape of bug:

1. **Self-wakeup deadlock** (`schedule_task` → `wait_task_off_cpu` spin).
   The wake path called from a timer ISR could target the
   currently-executing task on the same CPU; the spin waited for an
   `on_cpu` flag that only clears after the task yields, but the spin
   prevented the task from yielding. Self-deadlock.
2. **Wake-vs-block lost wakeups** (3 sub-races). A wake fired between
   `prepare_to_wait` and the WillBlock→Blocked CAS; the wake skipped
   the waiter; the waiter slept forever.
3. **Wake-during-block re-enqueue** (`run_ready_task_from_idle` tail).
   When a self-wake fired and the in-progress block then
   `unschedule_task`'d the entry, the task ended up Ready but in no
   runqueue. The dispatcher's tail had to learn to re-enqueue Ready
   tasks too.
4. **IRQ-disable around block** (`block_current_task*`). Even after
   the wake CAS was correct, the timer ISR firing mid-block exposed
   unrelated kernel races (state-3-for-task-4, page faults). cli
   around the state-CAS-through-yield window prevented the ISR from
   firing in that interval.
5. **Page-pollution → poison RIPs** (`mm/page_alloc.rs`). A kernel
   test wrote `(i & 0xFF) as u8` at offset `i` to a page, freed it
   without scrubbing; the buddy returned the dirty page to a
   subsequent caller; a `ret` decoded `0xd8..0xdf` at offsets
   `0xd8..0xdf` as a return address; CPU jumped to `0xdfdedddcdbdad9d8`.
   Fix: zero pages by default, opt-out via `ALLOC_FLAG_NO_INIT`.
6. **Slab/heap pollution** (`mm/kernel_heap.rs`). Same shape as #5
   but at the slab/magazine level — the page allocator's zeroing
   doesn't reach a recycled slab chunk. Fix: `kmalloc` zeroes by
   default, opt-out via `kmalloc_uninit`.

All six are correct fixes. **None of them are enforced by the type
system.** A future test that fills memory with a pattern, a future
caller of `schedule_task` that doesn't know about the on-cpu rule, a
future block path that forgets to `cli` — each would silently
re-introduce the same 12-hour debugging tax.

This plan moves each invariant from "convention" to "compile-time
guarantee" anchored in `slopos-ostd`. After it lands the kernel
literally cannot compile a violation.

## Status (as of 2026-05-10)

All five originally-planned phases have landed on
`sched/fix-self-wakeup-deadlock` (along with one design-gap follow-up
and a phase-2 cleanup).

| # | Item                                                                                                  | Status     | Commit       |
|---|-------------------------------------------------------------------------------------------------------|------------|--------------|
| 1 | `Frame<M, Init>` typestate (`Zeroed` / `Uninit`)                                                      | ✅ landed  | `1f88dcd4`   |
| 2 | Migrate kernel-stack / page-table / task-struct allocs to `Frame<_, Zeroed>`                          | ⚠️ partial | see below    |
|   | &nbsp;&nbsp;&nbsp;a. Delete `ALLOC_FLAG_ZERO` (zero-by-default)                                       | ✅ landed  | `1a63055d`   |
|   | &nbsp;&nbsp;&nbsp;b. Route plain `alloc_page_frame(0)` callers through typestate                      | ✅ landed  | `bfd4bd53`   |
|   | &nbsp;&nbsp;&nbsp;c. Frame `into_phys_release` interop bridge + `kmalloc_uninit` first hardened       | ✅ landed  | `05103eb2`   |
|   | &nbsp;&nbsp;&nbsp;d. Boot reorder so the typestate covers the bootstrap window (Phase 6 follow-up)    | ✅ landed  | `bcacfe3a`   |
|   | &nbsp;&nbsp;&nbsp;e. Delete `ALLOC_FLAG_NO_INIT`, delete `kmalloc_uninit`, add `no_pcp`/`dma` options | ✅ landed  | `15df1fb4`   |
|   | &nbsp;&nbsp;&nbsp;f. Hold `Frame<KernelMeta, Zeroed>` end-to-end (data structures store `Frame<_>`)   | ❌ not yet | architectural follow-up |
| 3 | `IrqDisabled<'cli>` capability for block paths                                                        | ✅ landed  | `af5e4db5`   |
| 4 | `PreparedWait` / `PublishedWait` state machine (migrated `task_wait_for`)                             | ⚠️ partial | `a800b9b5`   |
|   | &nbsp;&nbsp;&nbsp;Migrate `block_current_task_with_timeout` to the typestate                          | ❌ not yet | tractable follow-up |
| 5 | `WakeContext` parameter on `schedule_task`                                                            | ⚠️ partial | `0dee8889`   |
|   | &nbsp;&nbsp;&nbsp;Delete the `wait_task_off_cpu` self-skip defensive shim                             | ❌ not yet | tractable follow-up |
| 6 | Phase 6 (added during rollout): boot reorder so typestate covers bootstrap                            | ✅ landed  | `bcacfe3a`   |

Outstanding work, in tractability order:

- **Phase 4-tail**: migrate `block_current_task_with_timeout` (`core/src/scheduler/sleep.rs:328`) onto the `PreparedWait`/`PublishedWait` typestate. Plan called it out explicitly. ~one-session.
- **Phase 5-tail**: audit and delete `wait_task_off_cpu` (`core/src/scheduler/scheduler.rs:523`); the dispatcher-side re-enqueue check now covers both wake-context branches. ~one-session.
- **Phase 2f**: the architectural goal — data structures (page tables, task structs, kernel stacks) hold `Frame<KernelMeta, Zeroed>` end-to-end instead of raw `PhysAddr`. Multi-day ownership refactor; covers the long tail of `free_page_frame` callers (115 sites at last count). The current branch routes allocations through the typestate but releases the META_SLOT immediately via `into_phys_release`, so the type-system win at the alloc site doesn't carry into ownership.

After Phase 2f, the only `__alloc_page_frame*_raw` consumers in tree
will still be `install_meta_slots` (META_SLOTS bootstrap, fundamental
chicken-egg) and `LegacyFrameAllocShim::alloc` (typestate's own
backend) — which is the steady state.

## Scope

Four type-level enforcements, one per bug class. All four live in
`slopos-ostd` so that downstream kernel crates inherit them without
the option of bypass.

### 1. `Frame<M, Init: InitState>` — zero-by-construction ✅ landed (`1f88dcd4`)

`slopos-ostd::mm::frame::Frame<M>` already exists as a typed frame
wrapper keyed on metadata. Extend it with a second const-generic
state parameter that tracks initialisation:

```rust
pub mod init_state {
    pub struct Zeroed;
    pub struct Uninit;
    pub trait InitState: sealed::Sealed {}
    impl InitState for Zeroed {}
    impl InitState for Uninit {}
    mod sealed {
        pub trait Sealed {}
        impl Sealed for super::Zeroed {}
        impl Sealed for super::Uninit {}
    }
}

pub struct Frame<M: AnyFrameMeta, S: InitState = Zeroed> {
    paddr: Paddr,
    _meta: PhantomData<M>,
    _state: PhantomData<S>,
}

impl<M: AnyFrameMeta> Frame<M, Zeroed> {
    pub fn alloc(opts: FrameAllocOptions) -> Option<Self> { /* always scrubs */ }
}

impl<M: AnyFrameMeta> Frame<M, Uninit> {
    /// Hot-path opt-out. The caller certifies that the entire frame
    /// will be overwritten before any reader observes it.
    ///
    /// # Safety
    /// Caller's downstream writers must cover every byte of the frame.
    pub unsafe fn alloc_uninit(opts: FrameAllocOptions) -> Option<Self> { ... }

    pub fn scrub(self) -> Frame<M, Zeroed> { /* memset 0; cast state */ }

    /// Promote to Zeroed without scrubbing. Caller asserts the frame
    /// is already all-zero (e.g. fresh BSS-mapped page).
    ///
    /// # Safety
    /// All bytes must already read as zero.
    pub unsafe fn assume_zeroed(self) -> Frame<M, Zeroed> { ... }
}
```

Sites that need control-flow-sensitive memory (kernel stacks, page
tables, task structs) take `Frame<_, Zeroed>`. The compiler refuses
`Frame<_, Uninit>` until the caller scrubs or calls `assume_zeroed`.

The four-line `unsafe { Frame::alloc_uninit(...) }` per perf-critical
site becomes a documented audit point — exactly where you want the
kernel-team review to land.

**Migration**: ~30 page allocator call sites, ~80 `kmalloc` call
sites. Most become `Frame::<KernelMeta>::alloc(opts).into_phys()` (no
behaviour change), the dozen perf-sensitive ones get an explicit
`unsafe { ... }` block. No `ALLOC_FLAG_NO_INIT` flag needed —
deleted.

### 2. `IrqDisabled<'cli>` capability for block paths ✅ landed (`af5e4db5`)

The four `cli`'d block functions all share the pattern: the timer
ISR firing between the state-CAS and the `schedule()` yield is the
race. Today there is nothing stopping a future block path from
calling the `task_set_state_from_with_reason(... Blocked ...)` CAS
without a surrounding cli — the kernel would compile, the race would
return, and we'd debug it again.

Encode the requirement as a **lifetime-bound capability**:

```rust
pub struct IrqDisabled<'a> { _scope: PhantomData<&'a ()> }

impl IrqDisabled<'_> {
    /// Run `f` with `IRQs` disabled; the closure receives an
    /// `IrqDisabled` token whose lifetime is bounded by the call
    /// scope. The `cli`/`sti` is the only way to construct one.
    pub fn with<R>(f: impl for<'a> FnOnce(&'a IrqDisabled<'a>) -> R) -> R {
        let saved = save_flags_cli();
        // SAFETY: we just disabled IRQs.
        let token = IrqDisabled { _scope: PhantomData };
        let r = f(&token);
        restore_flags(saved);
        r
    }
}
```

Then change the unsafe block-state primitives so they only accept a
borrowed token:

```rust
pub fn block_current_task(_irq: &IrqDisabled<'_>) {
    // CAS WillBlock→Blocked, unschedule, schedule()
}
```

The compiler refuses `block_current_task()` outside an
`IrqDisabled::with(|tok| { ...; block_current_task(tok); ... })`
scope. The cli-disable becomes part of the type system, not a
"don't forget to wrap this" comment.

Borrowing rules also enforce that the token doesn't escape:
`block_current_task(token)` consumes the borrow for the call
duration; any code outside the closure cannot fabricate one.

### 3. `WaitFor<TargetId>` state machine for the wake-vs-block window ⚠️ partial (`a800b9b5`)

`task_wait_for` migrated. `block_current_task_with_timeout`
(`core/src/scheduler/sleep.rs:328`) still does the raw CAS-then-yield
pattern inside `IrqDisabled::with` — tractable follow-up.

The `task_wait_for` reorder was a series of "do step A before step B
before step C" rules. Encode the order as **type-state transitions**:

```rust
pub struct PreparedWait<'task> { current: &'task Task, target: TaskId }
pub struct PublishedWait<'task> { current: &'task Task, target: TaskId }

pub fn prepare_to_wait_for(target: TaskId) -> PreparedWait<'static> { ... }

impl PreparedWait<'_> {
    pub fn publish(self, current: &Task) -> PublishedWait<'_> {
        // sets `current.waiting_on = target`, fence
        ...
    }
}

impl PublishedWait<'_> {
    /// Returns Some if target is still alive; None if it terminated
    /// before we published. None caller must finish_wait + return.
    pub fn check_target(&self) -> Option<&'_ Self> { ... }

    pub fn block(self, irq: &IrqDisabled<'_>) -> WaitOutcome {
        // CAS WillBlock→Blocked, unschedule, schedule
        ...
    }
}
```

Method receivers move the typestate forward; you can't `.block()`
without a `PublishedWait`, you can't get a `PublishedWait` without
`.publish()` on a `PreparedWait`, and `.publish()` consumes the
`PreparedWait` so it can only happen once.

Combined with #2, the entire block sequence becomes:

```rust
IrqDisabled::with(|irq| {
    let wait = prepare_to_wait_for(child_pid)
        .publish(self_task);
    if let Some(armed) = wait.check_target() {
        armed.block(irq);
    }
});
```

A future block path that forgets `check_target` — as
the original `task_wait_for` did — won't compile.

### 4. `WakeContext::Self_` vs `WakeContext::Other` for `schedule_task` ⚠️ partial (`0dee8889`)

Enum + parameter live; every caller tagged. The plan also asked to
"Delete the self-skip defensive shim" — `wait_task_off_cpu`
(`core/src/scheduler/scheduler.rs:523`) is still called by
`schedule_new_task`. Tractable follow-up.

The self-wake bug was: `schedule_task(target)` from a wake path that
the kernel didn't realise might fire on the current-running task on
this CPU. Encode the distinction in the wake-call signature:

```rust
pub enum WakeContext {
    /// The wake path is allowed to spin on `target.on_cpu`. Caller
    /// proves it is NOT running on the same CPU as the target.
    OtherCpu,
    /// The wake path must NOT spin on `on_cpu`. The dispatcher's
    /// re-enqueue handles it instead. Used inside the timer ISR.
    SelfPossible,
}

pub fn schedule_task(target: *mut Task, ctx: WakeContext) -> c_int { ... }
```

The timer ISR's `wake_due_sleepers` always passes
`WakeContext::SelfPossible`. The IPI-driven cross-CPU wake passes
`OtherCpu`. The compiler doesn't enforce which is correct, but the
API forces the caller to **think** at every call site, and the
existing self-skip in `task_wait_off_cpu` becomes unnecessary —
`SelfPossible` simply doesn't call `wait_task_off_cpu`.

Stretch: encode the distinction in types instead of values, e.g.
`schedule_task_self_safe()` vs `schedule_task_cross_cpu(spin: SpinAllowed)`.

## Sequencing

Land in this order, smallest-blast-radius first:

1. **`Frame<_, Init>`** in OSTD. Pure additive: existing
   `alloc()`/`alloc_zeroed()` keep working as `Frame<_, Zeroed>`,
   the new `alloc_uninit()` is the explicit perf escape. ✅ landed
2. **Migrate** kernel-stack / page-table / task-struct allocations to
   `Frame<_, Zeroed>`. Delete `ALLOC_FLAG_NO_INIT`. Delete
   `kmalloc_uninit` (or make it the only path; rename `kmalloc`).
   ⚠️ flag/symbol deletions and the typestate routing landed; the
   end-to-end ownership migration (data structures hold `Frame<_>`
   instead of `PhysAddr`) is the remaining architectural follow-up.
3. **`IrqDisabled<'cli>`** in OSTD. Migrate the four cli'd block
   primitives to take it. Compile-time check that no caller forgets
   the cli scope. ✅ landed
4. **`PreparedWait`/`PublishedWait`** state machine. Migrate
   `task_wait_for` and `block_current_task_with_timeout` to use it.
   Delete the manual order-of-operations comment. ⚠️ `task_wait_for`
   migrated; `block_current_task_with_timeout` follow-up pending.
5. **`WakeContext`** parameter on `schedule_task`. Audit all callers,
   tag each as `OtherCpu` or `SelfPossible`. Delete the self-skip
   defensive shim. ⚠️ enum + caller tags landed;
   `wait_task_off_cpu` deletion follow-up pending.
6. **(Added during rollout) Boot reorder so the typestate covers the
   bootstrap window.** Split `init_memory_system`, move
   `install_meta_slots` and `register_with_ostd` between the halves,
   gate the raw buddy entry points behind `__alloc_page_frame*_raw`.
   ✅ landed

Each step is a separate PR. Each step deletes more legacy code than
it adds.

## What we don't fix (yet)

- **Stack-overflow page-fault on kernel stack guard.** Seen
  intermittently as `cr2=0xffffffffaXXXXXXXX` faults in
  `copy_nonoverlapping`. Distinct bug class; needs frame-pointer
  budget audit, not a type wrapper.
- **TCG-vs-KVM timing divergence.** Local TCG has ~30% spurious
  hangs; KVM is 100% green. Either a real but rare race or a TCG
  bug. Track separately.
- **CS 3 (Asterinas-style AP entry).** 9/10 KVM, occasional hang in
  the 10% case. Needs its own analysis; the type-level wins above
  may close the residual race.

## Outcome

After these five PRs:

- Reading uninit memory is impossible from safe Rust in `slopos-ostd`
  consumers.
- A block sequence that forgets `cli` does not compile.
- A wait sequence that races publish-vs-check does not compile.
- A wake path that doesn't think about self-vs-cross does not
  compile.
- The 12-hour debugging tax for this bug family is paid once, here,
  and never again.
