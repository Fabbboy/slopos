# The lock-order validator is off in every production boot

`slopos-ostd/src/sync/lock_graph.rs` is a real lockdep: dependency-edge learning,
cycle detection, a chain-hash cache, lock-free CAS bookkeeping. It is also
deterministically disabled before the kernel finishes initialising memory.

Every boot now says so — the four `GRAPH_OVERFLOW` latch sites emit a warning
naming the lock and the pool counts, `kdiag_dump_lock_graph` prints validator
state at the end of driver init, and `slopos_testing::zz_lockdep_tests` proves
the cycle detector fires and reports whether the boot reached the end with the
validator intact. What follows is the work that makes that report say `ACTIVE`.

## The mechanism

Class identity is the **lock instance address**. The module doc states this as a
deliberate simplification over lockdep's three-tier model, "adequate for SlopOS's
~45-lock kernel" (`:5-14`), and the table is sized to match:

```rust
/// Maximum distinct lock classes (one per unique lock instance address).
/// Kernel currently has ~45 locks; 10× headroom for growth.
pub const MAX_CLASSES: usize = 256;
```

Then `init_process_vm` does this (`mm/src/process_vm.rs:1863-1865`):

```rust
for i in 0..MAX_PROCESSES {
    PROCESS_VMS[i].lock().reset();
}
```

`MAX_PROCESSES` is 256. Each `PROCESS_VMS[i]` is a distinct lock instance, so
each is a distinct class. That single loop registers 256 classes and exhausts the
table during memory init — before the event bus, the TTY table, the TCP shards or
the futex buckets have acquired anything.

On exhaustion the validator turns itself off, silently (`:436-440`):

```rust
None => {
    // Class table full — disable validator gracefully.
    GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
    return;
}
```

No klog, no counter, no accessor for `CLASS_COUNT`/`EDGE_COUNT`/`CHAIN_COUNT`.
Once latched it stays off for the rest of the boot. Every lock acquired after mm
init is unvalidated, which is essentially every interesting lock in the kernel.

The static lock arrays that would need classes if the validator were alive are
far past 256 on their own: the event bus contains 544 `WaitQueue`s each embedding
a `SpinLock`, plus 64 futex buckets, 64 TCP shard slots, 32+32 TTY, 32 input, and
more. Classes are registered lazily on first acquire, so an unused queue costs
nothing — but the ordering is what matters here, and mm init gets there first.

## The second kill switch

`poison_unlock_all_held` sets `PANIC_BYPASS` permanently
(`slopos-ostd/src/sync/lock_graph.rs:635`):

```rust
PANIC_BYPASS.store(true, Ordering::Release);
```

Its doc calls this a one-way transition on the grounds that "the kernel never
resumes from panic". But `call_panic_cleanup` calls it
(`slopos-ostd/src/panic_recovery.rs:117-125`) and its own comment says it is
"invoked after `catch_panic!` catches a kernel-test unwind" — the kernel *does*
resume. `PANIC_BYPASS` is cleared only by `reset_for_test`, which is
`cfg`-gated.

So one recovered oops disables ordering validation for the remainder of the boot,
on top of the overflow latch.

## Why this matters here specifically

SlopOS has a documented history of exactly the bug class this validator exists to
catch: a buddy/slab cross-CPU TLB-shootdown deadlock (`plans/KNOWN_ISSUES.md`), a
`PROCESS_VMS` cli-lock held across an operation that re-enables interrupts, and
two separate lost-wake incidents. The tool that would have flagged the lock-order
half of those has been off the whole time.

It also means a green 2716-test suite carries less information than it appears
to: no ABBA inversion introduced anywhere after mm init can fail a test, because
nothing is checking.

## Fix

### 1. Static class keys (the real fix)

Give `SpinLock::new` a `&'static LockClassKey`, derived per *declaration site*
rather than per instance — so all 256 `PROCESS_VMS` slots share one class, all
544 event-bus wait queues share one, all 64 futex buckets share one. This is
exactly lockdep's `lockdep_set_class` / `LOCKDEP_STATIC_CLASS` mechanism and it
exists because Linux hit this identical problem with per-instance keys on arrays
of like objects.

Keep the instance address for the poison-unlock walk, which genuinely needs
per-instance identity.

With declaration-site classes, 256 is plausibly adequate again — but measure
rather than assume, and see item 2.

### 2. Split the panic bypass

`PANIC_BYPASS` conflates a genuinely-fatal abort with a recovered oops. Give the
fatal path the one-way transition it wants and clear the flag when an oops is
recovered. Until that lands, the fourth test in the suite disables ordering
validation for the rest of the run, and `zz_lockdep_tests` has to override the
gate rather than rely on it.

## Phases

| # | Work | Done when |
|---|---|---|
| 3 | `&'static LockClassKey` on `SpinLock::new`, declaration-site keys for the array-of-locks statics | `kdiag_dump_lock_graph` reports `ACTIVE` with a class count in the tens; `lockdep_graph_overflow_clear_at_boot_end` passes instead of skipping |
| 4 | Split `PANIC_BYPASS` so the recoverable-oops path clears it | A recovered test panic leaves validation enabled, and the ABBA self-test no longer needs `SelfTestGuard` to override the gate |
| 5 | Re-measure and re-size `MAX_CLASSES`/`MAX_EDGES` against the real post-fix counts | The sizing comment matches measured reality rather than an estimate |

The numbering is kept so cross-references elsewhere keep resolving.

## Risks

- **Phase 3 touches every `SpinLock::new` call site.** It is mechanical but wide.
  A `Default`-keyed overload can keep unconverted sites compiling during the
  migration, as long as it is removed at the end rather than left as an escape
  hatch.
- **A newly-alive validator will find real inversions.** That is the intended
  outcome, but it means phase 3 may turn the suite red, and the findings are the
  work rather than a regression. Budget for it — and note the two documented
  lock-order hazards in `KNOWN_ISSUES.md` are prime candidates to fire first.
  `RESERVED_TEST_CLASSES` holds four slots back from the registrable range, so
  the self-test keeps working whatever the class count becomes.
- **Per-acquire cost rises** once the validator is actually running, on every lock
  acquisition. Measure boot time and suite wall-clock before and after; if it is
  material, the answer is a build-time feature gate defaulting on for `just test`
  and off for `boot-prod`, not silently leaving it broken.
