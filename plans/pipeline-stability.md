# Making the pipeline stable

Root causes behind the flaky CI runs, and the fixes for them. Written against
`455102f9`, which is the tip of `develop` and is **red**.

---

## 0. What is actually failing

`ci.yml` on `develop`, 2026-08-10 → 2026-08-24: 30 runs, 14 green, 7 cancelled
(superseded force-pushes), **11 failed**. Grouped by cause rather than by test
name:

| Cause | Failing runs | Class |
|---|---|---|
| The kernel-I/O freeze window | 708, 711 (3 distinct tests) | flake |
| `utest_percore_reactor` — spurious `SIGSEGV` from a demand fault | 705 | flake, and a **kernel bug** |
| `utest_dns_resolve` — resolution through QEMU user-net | 697, 698 | non-hermetic |
| Lockdep **class** cap growth | 674, 686, 687, 692 | gate working; needed a rebaseline |
| `check_stack_sizes` on the release ELF | 692 | gate working |
| `utest_image` — a deleted asset | 696, 697 | ordinary breakage |
| `extractions/setup-just@v4` socket hang up | 678 | CI infrastructure |

Only the first three are flakes. The freeze family is the one that has `develop`
red now, and two successive fixes for it have each removed the assertion from one
test and watched the flake surface in the next test that freezes kernel-I/O.

## 1. The reproduction

CI runs a 4-vCPU guest on a 4-vCPU cloud runner, so the host can deschedule a
vCPU for tens of milliseconds. The published reproduction for that
(`plans/vcpu-steal-robustness.md` §1) is host spinners plus `taskset`:

```sh
just _build-run-tests
for j in $(seq 1 24); do ( while :; do :; done ) & done
taskset -c 0-3 builddir/run_tests --raw --no-color
```

**QEMU TCG is a second, cheaper reproduction of the same condition**, and it does
not need contention at all. TCG slows the *guest* relative to wall clock, which
is exactly what steal does: every in-guest wall-clock budget shrinks in
guest-instruction terms, and every window in which an external event can land
grows. Four plain TCG runs of the unmodified tree (no `/dev/kvm`, `-cpu max`,
`-smp 4`, 2972 kernel tests each):

| Run | Failures |
|---|---|
| 1 | `napi_tests::test_recv_timeout`, `napi_tests::test_send_backpressure`, `packetbuf_tests::test_drop_multiple`, `packetbuf_tests::test_pool_exhaust_and_recover` |
| 2 | `tcp_keepalive_tests::test_keepalive_max_probes_rst` |
| 3 | `tcp_keepalive_tests::test_keepalive_reset_on_data`, **`sched_tests::test_remote_inbox_drops_non_ready_tasks`** |
| 4 | `napi_tests::test_recv_timeout`, `napi_tests::test_send_backpressure`, `tcp_keepalive_tests::test_keepalive_max_probes_rst`, **`sched_tests::test_remote_inbox_drops_non_ready_tasks`** |

Run 3 and run 4 reproduce the exact CI failure that has `develop` red, with the
same log lines. Under 12 host spinners the same runs additionally reach
`unwind_index_tests::test_unwind_lookup_is_indexed` and
`rcu_cb_tests::test_synchronize_rcu_allocates_nothing`.

## 2. Root causes

### 2.1 The kernel-I/O freeze is cooperative, and no amount of waiting fixes that

`request_kernel_io_freeze` (`slopos-ostd/src/sync/kernel_io_task.rs:109`) *wakes*
every registered kthread. A woken thread is `Ready` and must be **dispatched**
before it can reach `hold_frozen` and count as frozen. `freeze_kernel_io_all`
waits 50 ms of wall clock for that to happen. A host that deschedules the vCPU
carrying the thread outlasts any such window, and the guest cannot tell that
apart from a wedged thread.

What the incomplete freeze leaves behind is the actual defect:

- `ReadyQueue::clear_with_ref_release` (`sched/src/per_cpu.rs:59-88`) deliberately
  **re-links** registered kernel-I/O tasks, and `KernelTestScope::enter` never
  clears the queues at all (`init_all_percpu_schedulers` is `init_once`). So a
  thread the freeze failed to catch sits `Ready` in a runqueue for the whole
  test. That is `ready_count=1` in
  `test_remote_inbox_drops_non_ready_tasks`, and it is "runnable privileged
  work" in `test_low_priority_is_not_starved_by_busy_normal`.
- `scheduler_timer_tick` calls `drain_remote_inbox()` and `wake_due_sleepers()`
  **above** the `SCHEDULER_ENABLED == 0` early return
  (`sched/src/scheduler.rs:1790-1795`), so the BSP's own 100 Hz tick republishes
  work into the local runqueue *inside* a scope.
- `reset_sleep_queue` is `reset_preserving(&kernel_io)`
  (`sched/src/sleep.rs:526`), so kernel-I/O deadlines survive the registry reset
  and can fire mid-test.

The scope's contract is that nothing races the test body. It does not hold for
infrastructure work, and every test that assumes a quiet runqueue is a flake by
construction.

### 2.2 A transient failure to get exclusive access to an address space kills the process

`vm_space_get_mut` (`mm/src/user_mappings.rs:84`) spins 1,000,000 times waiting
for `KArc::strong_count(vm_space) == 1 && weak_count == 0`, then returns
`MapError::ConcurrentAccess`. `demand::handle_demand_fault` turns that into
`MmError::MappingFailed`, and `mm/src/page_fault.rs` turns *that* into
`FaultOutcome::Fatal(TaskFaultReason::UserPage)` — `SIGSEGV`, `code=139`, to a
correct program.

The second reference is ordinary: every syscall that copies user memory clones
the `KArc` (`mm/src/user_copy.rs:52`, `core/src/syscall/context.rs:133`) and
holds it for the copy. A sibling thread faulting while that copy is in flight
loses. Preemption is enough; steal only widens it. This is CI run
`32659951417`, and it is a kernel bug independent of the test suite.

### 2.3 Kernel net tests race, and perturb, a live network

`scripts/qemu_run.sh` attaches `-netdev user,... -device virtio-net-pci` in
**every** mode, tests included, so the guest always has a live QEMU slirp
network with a gateway that answers.

- `napi_tests` and `tcp_keepalive_tests` call
  `socket_connect(sock, [10,0,0,2], 80)` and then fake an established connection
  by injecting a synthetic SYN-ACK through `tcp::input`. The real SYN goes out on
  the wire; slirp answers asynchronously; the PCB the test is asserting on
  changes underneath it.
- `packetbuf_tests` assert absolute values of the **global** `PACKET_POOL` while
  the live stack allocates from it (observed: `expected 255, got 256`), and
  `test_pool_exhaust_and_recover` deliberately exhausts that pool, which starves
  the live stack for as long as it holds it.
- `tcp_keepalive_tests` installs a **global** `MockClockGuard`, advances mock
  time by 7200 s and calls `NET_TIMER_WHEEL.process_due()`, which fires every
  unrelated timer in the wheel.

### 2.4 Timing assertions written as a single sample, a mean, or an absolute budget

A wall-clock bound measures the host as much as the kernel. The tree has these
in three shapes, all of which fail on a stolen vCPU while the code under test
is correct:

- a single differential sample (`unwind_index_tests::test_unwind_lookup_is_indexed`),
- a mean over a loop with interrupts on (`sched_tests::test_quota_charge_cost`,
  which `check_quota_headroom.sh` enforces as a hard cap),
- an absolute ceiling (`rcu_cb_tests::test_rcu_drain_never_waits_for_a_grace_period`,
  `syscall::tests::test_unix_socket_poll_syscall_e2e`).

The durable form is a **minimum over repetitions** — one clean pass is enough and
no number of stolen ones can lower it — or, better, an assertion on the
structural fact the timing was standing in for.

### 2.5 Absolute assertions on global counters

A test that reads a machine-wide counter is asserting about every CPU. The
suite does this against the packet pool, the buddy allocator's free count, the
bottom-half drain counters (`slopos-ostd/src/sync/bh.rs:51,59` — global, not
per-CPU), the kconsole pending bitmask, the live event bus, the oops ledger, and
the stack-VA in-use bitmap. Most have a per-CPU or per-principal equivalent
that is both sound and more sensitive.

---

*Sections 3 (the fixes) and 4 (verification) follow as they land.*

---

### 2.6 One documented failure that no longer reproduces

`userland/src/bin/tests/dns_resolve_test.rs` carries a header saying the test is
**"Known-failing in a full-suite boot, and deliberately not excused"**, because
"running the kernel-phase AF_UNIX socket tests first leaves UDP DNS dead for the
rest of the run". That does not reproduce. Three probes — the DNS utest alone,
`test_unix_socket_send_recv_basic` then the DNS utest, and the whole
`test_unix_socket_*` group then the DNS utest — all resolved, in 71 ms and
80 ms. The most likely explanation is `e066c7d` ("Reapply fix(virtio-net): drain
RX while waiting for a DNS reply"), which landed after that header was written.

A comment telling the next reader that a green test is known-failing is a defect
of its own: it invites them to ignore a real failure. It is corrected rather
than deleted, so the history stays legible.


## 3. The fixes

Each is a mechanism change, not a widened bound. Where a test asserted more than
the kernel promises, the kernel's real contract is stated and asserted instead.

### 3.1 The test scope holds the kernel-I/O threads off every run queue

`SchedPlacement::Held` is a new placement meaning *no scheduler container owns
this task, and the hold will publish it itself*. `KernelTestScope::enter`, once
the AP pause has established that no AP is dispatching, arms a hold and sweeps
every CPU's ready queue and remote inbox; `Drop` republishes everything it took.
While the hold is armed, every publication path — `schedule_task`, the inbox
drain, both queue-clearing paths — claims a covered task into `Held` instead of
linking it.

The load-bearing observation is that this takes nothing away. During the kernel
test phase the BSP has no idle task, so `schedule_internal` returns without
switching, and the APs are paused: **no task other than the test task can run
anywhere for the scope's duration, frozen or not.** A thread the cooperative
freeze failed to catch was never going to be dispatched — it was only
*recorded* as a ready-queue member, and `total_ready_count()` and
`fair::tier_owed` read that record. The hold fixes the bookkeeping, and it does
so without depending on a thread running, which is the thing a stolen vCPU can
prevent.

`kernel_io_is_frozen()` becomes `kernel_io_is_quiesced()`, measured from
placements rather than trusted from the freeze's own report. `FreezeOutcome`
keeps its exact present meaning: it is the freeze's *observation* of whether a
thread parked under its own power, and that diagnostic is worth keeping.

This also closes a latent strand: `force_clear_inbox_count` discarded inbox
entries with none of the kernel-I/O preservation its ready-queue sibling
performs, leaving a thread `Ready` with `SchedPlacement::None` — a state no wake
re-publishes and only the idle rescue sweep recovers.

### 3.2 A page fault never turns a transient conflict into a fatal one

`MapError::ConcurrentAccess` becomes `MapError::WouldBlock`, `MmError` gains
`Retry`, and `FaultOutcome` gains `Retry`, consumed at
`boot/src/idt.rs` by requesting a reschedule and returning to `IRET`. `#PF` is a
fault, so the saved `RIP` still names the faulting instruction: it re-executes
and the decision is retaken. Nothing is mapped, nothing stays allocated, no
generation is bumped, and no fault reason is recorded, so the retry is free of
side effects.

Two supporting changes. The spin drops from 1,000,000 iterations to 64 — one
cache-line round trip rather than a scheduling quantum, because past that the
holder is descheduled and every further iteration burns the CPU it needs in
order to release, with interrupts and preemption off under the per-process lock.
And the comment claiming a caller had violated an "external-lock contract" is
deleted: the clone it blames is the documented design, minted deliberately so
`user_copy`'s walk runs with the per-process lock released.

There is **no escalation after N retries**. Any threshold is a re-measurement of
the host-scheduling variance being removed, and would reintroduce a rare
`SIGSEGV` on a correct program. A retry that never terminates is a leaked
address-space handle — a kernel defect — and the per-CPU episode tracker warns
once, loudly, when one lasts past 50 ms, rather than killing the process that
noticed.

### 3.3 The net tests get a hermetic destination

A test-only blackhole `NetDevice`, an RFC 5737 TEST-NET address on it, a route,
and a pre-seeded neighbour entry so no ARP is emitted. Route lookup and the
neighbour cache are the only things that pick a device, so a test's SYN provably
cannot reach the real NIC, and an ingress gate keeps a real inbound frame from
reaching a test's PCB. The pool-accounting tests move to a private `PacketPool`
so they neither observe nor starve the global one, and the timer-wheel and mock
clock are scoped so a test that advances mock time by two hours cannot fire and
discard the live stack's timers.

### 3.4 Timing assertions become minima, or stop being timing assertions

A wall-clock bound measures the host as much as the kernel, and both the TSC and
the HPET keep counting through a vCPU deschedule. Where the measurement is the
point, the durable statistic is the minimum over repetitions: one clean pass is
enough and no number of stolen ones can lower it. Where the timing was standing
in for a structural fact — "the drain never waited for a grace period", "poll did
not sleep", "the wait took the fast path" — the structural fact is asserted
instead.

The harness's own clock was part of this: `estimate_cycles_per_ms` trusted
CPUID leaf 0x16, which is absent under TCG, and fell back to a hardcoded 3 GHz,
so every reported `time_ms` was wrong by whatever the real ratio was. Per-test
elapsed time now comes from the monotonic clock directly.

### 3.5 Absolute assertions on machine-wide counters become local ones

The bottom-half drain counters were global, so one CPU's ordinary drain passed as
evidence about another's — and in one direction it *masked* a real failure.
They are per-CPU now. The same treatment applies across the suite: per-process
quota accounts instead of the global buddy free count, the epoch an operation
actually closed instead of two independent reads of a shared counter, the frames
a test itself allocated instead of a hand-picked slack constant.

### 3.6 The gates stop failing on scheduling, and the harness stops missing a dead machine

Lock **class** counts are deterministic — a class registers on first acquire of a
declaration site — and their exact caps caught three of the four ratchet
failures in the window. They stay exact. Edge and chain counts in the two test
phases move with scheduling, and the gate file itself records the unmodified
tree exceeding its own cap; those become bands that report drift and fail only
on the fill ceiling, the class caps, a dead entry, or an actual violation. The
cycle detector, which is the correctness check, is untouched.

The Go harness learns the kernel's abort banner: it tightens its silence budget
rather than exiting, because in the recorded case the abort was one CPU and the
surviving BSP produced 2931 further results.

### 3.7 Real bugs fixed along the way

- **A demand fault could `SIGSEGV` a correct multithreaded process** (§2.2).
- **The quota ledger published `used` before `peak`**, so a concurrent
  `ledger_audit` scan could observe `used > peak` — the exact invariant the
  audit exists to check, violated by the writer's own publish order.
- **`test_gdt_set_ist_valid_indices` installed a read-only `.rodata` static as
  the IST top for seven vectors**, two of which — `PageFault` and `KeyboardIrq` —
  are IST-routed. A fault in that window pushes an exception frame onto
  read-only memory, faults, double-faults onto the same stack and triple-faults.
- **`test_quota_custody_charges_the_sender` leaked two AF_UNIX endpoints, an fd
  and a task on its success path**, permanently consuming slots from a fixed
  pool on every run.
- **The net stack had two time bases**: `socket_process_timers` read
  `kernel_services::clock::uptime_ms()` while the deadlines it compared against
  came from `net::clock::now_ms()`.

---

## 4. Found, argued, and deliberately not fixed here

Each of these is real, each has an argument, and each is left with the argument
recorded rather than acted on.

**`try_charge` debits leaf-upward.** `charge_row(child)` completes before
`charge_row(parent)`, so between them the child has grown and the parent has
not: `ledger_audit`'s `AncestorUnderCount` is transiently true on every
hierarchical charge. The refund direction is already safe. The mechanical fix —
collect the chain and debit root-downward — was implemented, measured and
reverted, because it moves two things the fix itself cannot re-measure: a batch
refused at the leaf would then have been charged to and unwound from every
ancestor, raising the root's `peak` against the *exact* per-account caps in
`scripts/gates/quota/tests.txt`; and `refused_by` flips from the nearest
refusing row to the outermost one, taking the `denials` attribution with it.
It also would not make the audit's check sound, because the ancestor and its
children are sampled at different times either way — `AncestorUnderCount` is a
quiescent-ledger check, and the honest fix for the *test* that asserts it is to
run it quiesced.

**`account_release` handed its outstanding amount up while the row was still
live**, so the parent sat below the sum of its children for the length of the
hand-up. That one *is* fixed here — the row goes dark first — because it is a
genuine state violation rather than a sampling artifact, and going dark is
precisely what makes an in-flight refund a whole-chain no-op.

**A double-free on a non-`WouldBlock` cursor error.** In
`ostd_map_4kb_user`, if `cursor_mut` or `map` fails after a `UFrame` has been
wrapped, the frame's drop frees the page and the caller's `free_page_frame`
frees it again. Pre-existing, not made worse by the retry work — the hoist there
closes only the arm the retry work makes reachable. The fix is to have `map`
hand the frame back on error.

**An unprotected window in `__spawn_kernel_io`.** The task is published to the
registry before its id is bound to the stop, so between those two lines a live
kernel-I/O thread is preserved by neither `task_registry_reset` nor
`reset_preserving`. Not currently reachable — all three spawns happen on the
BSP — but nothing enforces that.

**The guarded skip for an AP pause that proves the target took no CPU.** The
classification (`NotParking`, which is always a bug, versus `NotRunning`, which
is not evidence) is worth having and is built. Turning `NotRunning` into a
`Skipped` rather than a panic is not, yet: `TestOutcome::is_pass()` counts
`Skipped` as a pass and `summary.skipped` is never incremented at all, so an AP
genuinely wedged with `IF=0` would silently skip every hermetic test and the
suite would go green with zero coverage. The counting bug is the prerequisite,
and the skip should be gated on `hypervisor_present()` so bare metal — where a
stalled heartbeat *is* evidence — still fails.
