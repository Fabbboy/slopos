# Making the pipeline stable

Root causes behind the flaky CI runs, and the fixes for them. Started against
`455102f9` — the tip of `develop`, and **red** — and carried through the work
that landed in `a8d5b17`, `3f2acdd`, `c860a27` and the commit this document
ships in. §5 records what the fixes measure.

---

## 0. What is actually failing

`ci.yml` on `develop`, 2026-08-10 → 2026-08-24 (runs 681–711): **30 runs, 14
green, 7 cancelled** (superseded force-pushes), **9 failed** — 686, 687, 692,
696, 697, 698, 705, 708, 711. Grouped by cause rather than by test name; a run
that failed for two reasons appears twice:

| Cause | Failing runs | Class |
|---|---|---|
| The kernel-I/O freeze window | 708, 711 (3 distinct tests) | flake |
| `utest_percore_reactor` — spurious `SIGSEGV` from a demand fault | 705 | flake, and a **kernel bug** |
| `utest_dns_resolve` — resolution through QEMU user-net | 697, 698 | non-hermetic |
| Lockdep **class** cap growth | 686, 687, 692 | gate working; needed a rebaseline |
| `check_stack_sizes` on the release ELF | 692 | gate working |
| `utest_image` — a deleted asset | 696, 697 | ordinary breakage |

Only the first three are flakes. The freeze family is the one that has `develop`
red now, and two successive fixes for it have each removed the assertion from one
test and watched the flake surface in the next test that freezes kernel-I/O.

(An earlier draft of this section said "11 failed" and cited runs 674 and 678,
which are outside the window it names. The numbers above are read back from
`ci.yml`'s run list.)

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
same log lines. A run under 12 host spinners additionally reached
`unwind_index_tests::test_unwind_lookup_is_indexed` and
`rcu_cb_tests::test_synchronize_rcu_allocates_nothing` before it was cut short —
so those two are reproductions, not a claim about the whole contended suite. (12
spinners, not the 24 the recipe above shows: the recipe is the published one,
the number used here was halved to keep the run finishing.)

## 2. Root causes

*Line references in this section are as of `455102f9`, the commit the analysis
was written against. Several of the files named have since been changed by the
fixes in §3.*

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

The shape reaches into the gates too, and further than the suite: an absolute
*cycle* budget is a wall-clock budget with the units changed. `check_quota_headroom.sh`
capped the charge path at 3000 and 9000 cycles, numbers taken on a KVM host; the
identical unmodified tree measures 20631 and 81886 under TCG and fails the gate
on every machine without `/dev/kvm` (§5).

### 2.5 Absolute assertions on global counters

A test that reads a machine-wide counter is asserting about every CPU. The
suite does this against the packet pool, the buddy allocator's free count, the
bottom-half drain counters (`slopos-ostd/src/sync/bh.rs:51,59` — global, not
per-CPU), the kconsole pending bitmask, the live event bus, the oops ledger, and
the stack-VA in-use bitmap. Most have a per-CPU or per-principal equivalent
that is both sound and more sensitive.

### 2.6 One documented failure that no longer reproduces

`userland/src/bin/tests/dns_resolve_test.rs` carries a header saying the test is
**"Known-failing in a full-suite boot, and deliberately not excused"**, because
"running the kernel-phase AF_UNIX socket tests first leaves UDP DNS dead for the
rest of the run". That does not reproduce. Two probes — `test_unix_socket_send_recv_basic`
then the DNS utest, and the whole `test_unix_socket_*` group then the DNS utest
— both resolved, in 71 ms and 80 ms. (A third, the DNS utest alone, is what an
earlier draft counted; it never ran, because that build failed.) The most likely explanation is `e066c7d` ("Reapply fix(virtio-net): drain
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

Three properties of the cover set are load-bearing, and each one is a strand
the first cut of the hold could produce:

- **Arming unions, never replaces.** The depth counter nests; the id set has to
  nest with it. An inner arm taken after a stop deregistered — or before one
  bound its id, which `KernelIoStop::task_id` leaves invalid until the thread
  first runs — used to drop that id, so the outermost disarm never visited it
  and the task stayed `Held` on no queue. `placement_is_durable_owner(Held)` is
  `true`, so `rescue_check_task` skips it and `strand_sweep_task` (which keys on
  `None`) says nothing: it is gone until reboot.
- **The test helper is additive too**, for the panic path specifically.
  Unwinding runs `KernelIoHold::drop` — taking the depth 2 -> 1, republishing
  nothing — *before* `clear_kernel_io_hold_after_panic` snapshots the set. A
  displacing helper leaves that snapshot holding only the test's synthetic ids
  and every real kernel-I/O thread stranded.
- **The quiesced predicate walks the registry, not the arm-time snapshot**, and
  the settle loop refreshes the cover each round. Answering off the snapshot
  makes a thread that registered after the arm invisible to the predicate while
  it stays fully queueable — the predicate reports quiesced and the caller races
  exactly the thread it asked about, which is the original bug wearing the
  fix's clothes.

### 3.2 A page fault never turns a transient conflict into a fatal one

`MapError::ConcurrentAccess` becomes `MapError::WouldBlock`, `MmError` gains
`Retry`, and `FaultOutcome` gains `Retry`, consumed at
`boot/src/idt.rs` by requesting a reschedule and returning to `IRET`. `#PF` is a
fault, so the saved `RIP` still names the faulting instruction: it re-executes
and the decision is retaken. Nothing is mapped, nothing stays allocated, no
generation is bumped, and no fault reason is recorded, so the retry is free of
side effects.

One supporting change: the comment claiming a caller had violated an
"external-lock contract" is deleted. The clone it blames is the documented
design, minted deliberately so `user_copy`'s walk runs with the per-process lock
released.

`VM_SPACE_MUT_SPINS` deliberately stays at 1,000,000. Cutting it to 64 was
tried and reverted: only two of the ten `vm_space_get_mut` callers can retry —
the two the `WouldBlock` path reaches. The other eight (fork's COW marking,
`mmap`, `brk`, `mprotect`, `munmap`) turn a spin exhaustion into a returned
error, and `fork()`'s has no rollback, so it would leave the parent's pages
COW-marked with the child never created. A shorter spin is right *for the
retrying callers* and wrong for the rest; making it right for both is a
per-caller budget, not a global constant, and is not attempted here.

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

`10.0.0.1`/`10.0.0.2` were the last hole, and a wide one: every connect in
`socket_tests.rs` and every retransmit driven by `tcp_common.rs` really put a
frame on the virtio device and QEMU's gateway really answered it. A synthetic
PCB on a 4-tuple the wire can reach is a PCB the wire can tear down between the
injection and the assertion — which is what
`test_tcp_shutdown_wr_recv_still_works` was losing to.

**The address class is not what fixes that**, and the first cut of this work got
it wrong. DHCP installs a `0.0.0.0/0` default route, so `192.0.2.2` falls
through to the physical NIC exactly as `10.0.0.2` did; longest-prefix finds no
`/24` for either. The only thing that changes the outcome is the scope's own
metric-0 `/24` at the blackhole sink. The constants matter because a test's
4-tuple has to match the one the scope routes — they are not, by themselves, a
hermeticity property, and a comment claiming otherwise was in the tree until an
adversarial review of the sweep caught it.

So the criterion is not "does this test read a global table" but **"does this
test leave live PCB state the kernel's own threads can act on after it
returns"**. One injected data segment sets `delayed_ack_deadline_ms = now_ms +
DELAYED_ACK_MS`, and the net-timer kthread compares that against *real* uptime —
so a deadline computed from a test's `now_ms = 0` is already in the past when
the test returns, and the ACK goes out within one 50 ms kthread period, every
run. Inflight data with an armed `TcpRetransmit` is the same shape bounded by
`INITIAL_RTO_MS`. Both are transmits attributable to a test that has already
reported `ok`.

Two more mechanisms had to move with them, both found by reproducing the flake
under host contention rather than by reading:

- **`socket_process_timers` is gated.** It walks every PCB in the global table
  and can transmit, and it now reads the same clock a test advances, so a test
  that jumps mock time made every armed delayed ACK in the table look due to the
  net-timer kthread on another CPU. Without the gate, moving it onto the net
  clock (§3.7) would have *widened* a race rather than closing one.
- **`tcp_common::dispatch_fired_timers` drains the selector**, not
  `NET_TIMER_WHEEL` directly. Under a scope a `tcp::schedule` lands in the test
  wheel; draining the live one would fire nothing the test armed while leaving
  what it armed for the live thread to fire instead. That is how
  `test_tcp_delayed_ack_timeout` and `test_tcp_recv_updates_window` failed under
  contention having passed in five consecutive uncontended runs.

`poll_loopback` is gated too. It is the one ingress path that calls
`ipv4::handle_rx` directly rather than through `ingress::net_rx`, so a loopback
frame queued before a scope opened was delivered into the global TCP table while
the scope was up.

### 3.4 Timing assertions become minima, ratios, or structural facts

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

Where a cost genuinely has to be bounded, the durable form is a **ratio between
two quantities measured in the same run** — the emulation factor divides out.
The quota gate's charge cost is now depth 7 against depth 1: measured at
3.69–3.99 across five runs and three tree states, which is the tightest quantity
in any gate file here, and tight *because* it is a ratio (§5). Where even that
is unavailable, the honest move is a coarse ceiling that no host reaches, stated
as such: `test_rcu_drain_never_waits_for_a_grace_period` keeps its exact
`synchronize_rcu`-entry count as the real check and carries a 2000 ms ceiling
only to catch the regression the counter cannot see — an inline wait that never
calls `synchronize_rcu` at all.

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

Two things a band must not quietly become. It must not become a hole — a pool
that stopped being counted reads as maximally healthy against every ceiling
above it, so `min-edges` and `min-chains` join `min-classes` as explicit floors.
Those floors are deliberately *not* the band's low end: a run that happened to
observe fewer orderings is exactly as innocent as one that observed more, which
is the entire premise of banding, so a below-band value drifts like an
above-band one and only a value near zero fails. And it must not become a
silence — `DRIFT` moved to stderr, where every other diagnostic in the script
already goes, and a phase that drifted is summarised `DRIFT:` rather than `OK:`,
because the last word on a moved pool was otherwise "OK" on a green CI job's
collapsed log. The gate file now states the bound it actually enforces
(`max-fill-pct`, ~3.5x observed) rather than the band width, and the self-test
counts its own cases instead of carrying a hand-maintained tally that had
already drifted from 16 to 19.

The Go harness learns the kernel's abort banner: it tightens its silence budget
rather than exiting, because in the recorded case the abort was one CPU and the
surviving BSP produced 2931 further results. `Skipped` also stops counting into
`summary.passed` and gets its own `summary.skipped`, so a run that skipped
everything can no longer report as a run that passed everything.

### 3.7 Real bugs fixed along the way

- **A demand fault could `SIGSEGV` a correct multithreaded process** (§2.2).
- **The quota ledger published `used` before `peak`**, so a concurrent
  `ledger_audit` scan could observe `used > peak` — the exact invariant the
  audit exists to check, violated by the writer's own publish order. Reordering
  the two stores is *not* the fix: a charge that raises `peak` and then loses
  its CAS leaves the mark at a value never held, and one lost CAS whose retry
  succeeds is enough (`used=100,peak=100`; A raises to 101; B refunds 50; A
  retries and commits 51; `peak` stays 101). `check_quota_headroom.sh` caps
  peaks *exactly*, so a `+1` on a contended root row is a red gate. `used` and
  `peak` are now one `AtomicU64` — `used` in the low half, `peak` in the high —
  moved by a single CAS, so `used <= peak` holds by construction and only a
  committed charge can move the mark. `UsedAbovePeak` goes from "eventually
  true" to unobservable, which is why the two tests that assert `ledger_audit`
  need no quiescing.
- **`account_release` handed its outstanding amount up while the row was still
  live**, so the parent sat below the sum of its children for the length of the
  hand-up (see §4 for why the sibling ordering issue is *not* fixed).
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
- **`BuddyAllocator::stats()` read the per-CPU cache total outside the buddy
  lock**, while every path that drains a magazine into the free lists does both
  halves under it — so a peer draining in that window was counted twice and
  `free` could exceed `total`.
- **Seven TCP timestamp tests had never run.**
  `net/src/tests/tcp_timestamp_tests.rs` carried seven `pub fn test_*` and zero
  `stest!` registrations, so timestamp negotiation, PAWS and RTTM were untested
  in a tree that implements all three. The module compiled — it is declared in
  `tests/mod.rs` — which is exactly why nothing noticed. Registered here; the
  test-count ratchet moves by seven.
- **`virtio_net_transmit` and `transmit_udp_packet` never counted what they
  sent.** `NetDevice::tx` and `submit_tx_zerocopy` both bump
  `TX_PACKETS`/`TX_BYTES`; the two paths that reach the ring *without* going
  through `NetDevice::tx` did not, so every DNS query and every frame the
  service path sent was invisible to `ip -s` and to `sysmon`. Found by a
  rewritten `test_tx_fire_and_forget` asserting on the counter the submit was
  supposed to move, and failing deterministically in three runs; the second path
  turned up while fixing a test whose assertion was literally `ok || !ok`.
- **The reclaim tier's budget was specified in pages and enforced in blocks.**
  `Reclaimable::reclaim`'s contract is "at most `want` pages";
  `QuarantineReclaim::reclaim` forwarded `want` straight to
  `quarantine_release_some`, whose loop counted *blocks* and whose return
  counted pages. A single order-2 block in the backlog therefore made
  `reclaim::run(1)` return 4, and the assertion that had guarded the contract
  (`freed <= want`) was a latent false failure that only stayed quiet because
  the test before it drained the backlog. `quarantine_release_some` now budgets
  in pages. Declining a block that would overshoot is deliberately *not* the
  answer — that reclaimer would report zero forever while `reclaimable_pages`
  said there was work — so the contract states the residual instead: the total
  may exceed `want` by less than one unit, once for the whole call.
- **An active-open TCP connection never retransmitted its SYN.**
  `SynSentState` has carried `retransmits` and `retransmit_token` since it was
  written and nothing ever moved either: `tcp::connect` armed no timer, and
  `on_retransmit` matched `PcbState::Data` alone, so a `SynSent` PCB whose timer
  did fire fell through to `Outcome::Skip`. The SYN went out exactly once. One
  SYN lost to a full queue therefore stalled the connect for the socket layer's
  entire 30 s deadline, and an unreachable peer for all of it — the failure mode
  looked like a slow network rather than a missing retransmission. `connect` now
  arms the timer once the SYN is on the wire (the RTO is measured from the
  transmission, and the wheel must not be entered under the socket table lock),
  `on_retransmit` rebuilds and re-sends the SYN under the original ISS with the
  RTO doubling each time, and the attempt is abandoned after
  `ACTIVE_SYN_RETRIES_MAX` — releasing the PCB, which a `SynSent` entry nothing
  retires would otherwise hold along with its ephemeral port for the life of the
  boot. `on_retransmit`'s `Option<ConnId>` return became a three-way
  `RetransmitAction`: collapsing "re-send these buffered bytes" and "re-send
  this rebuilt segment" into one `Option` is what let the SYN case go unwritten
  without a compile error.
- **`utest_curl_e2e` could lose the whole run rather than fail.** Each of its two
  cases dials three unreachable-in-CI addresses in series with a blocking
  `connect`, and printed nothing until the case ended — so on a machine with no
  egress the harness saw more silence than its budget allows and killed QEMU,
  taking every test after it. The attempt is now announced before it blocks. The
  test still fails where there is no egress, which is correct; what it no longer
  does is convert an environment limitation into a lost run.

---

## 4. Found, argued, and deliberately not fixed here

Each of these is real, each has an argument, and each is left with the argument
recorded rather than acted on.

**`try_charge` debits leaf-upward.** `charge_row(child)` completes before
`charge_row(parent)`, so between them the child has grown and the parent has
not: `ledger_audit`'s `AncestorUnderCount` is transiently true on every
hierarchical charge. The refund direction is already safe. The mechanical fix —
collect the chain and debit root-downward — was implemented, measured and
reverted.

The decisive reason is the third one, not the first two. **It would not make the
audit's check sound**: `ledger_audit` samples the ancestor at one instant and
its children at a later one, so a hierarchical charge completing inside that
window makes `ancestor(T1) < Σ children(T2)` whichever direction the writer
walks, and reading children-first has the symmetric problem with refunds.
`AncestorUnderCount` is a quiescent-ledger check, full stop — unlike
`UsedAbovePeak`, which the packing above makes instantaneously true and which is
therefore the one invariant of the four a lock-free reader may hold to.

**A simultaneous open's SYN-ACK is never retransmitted.** `SynRecvState` carries
the same pair of dead fields the active-open fix revives — `retransmits` and
`retransmit_token`, written once at construction and never read — and the
`SynSent` → `SynRecv` transition arms nothing. The ordinary passive open is not
affected: a listener's SYN-ACKs go through `SynQueue::on_retransmit` on a
`TcpSynAck` timer, which does back off and does give up. What is uncovered is
only the simultaneous open, where both peers send a SYN and neither is a
listener. Fixing it is the same shape as the active-open fix, but the case it
serves is a pair of hosts dialling each other in the same round trip, and this
tree has no test that reaches `SynRecv` from `SynSent` on a live stack — so the
fix would ship with the same unfalsifiable assertion §6 exists to reject. Left
here with its shape written down rather than guessed at.

The two costs are real but secondary: `refused_by` would flip from the nearest
refusing row to the outermost one, taking `denials` attribution with it; and
every ancestor would be charged and unwound for a batch the leaf refuses,
raising the root's `peak` against the *exact* per-account caps in
`scripts/gates/quota/tests.txt`. Note the packing does not remove that second
one — the ancestor really would hold the batch for the length of the walk, so
the peak it records is honest and the caps simply have to be re-measured with
it.

**A double-free on a non-`WouldBlock` cursor error.** In
`ostd_map_4kb_user`, if `cursor_mut` or `map` fails after a `UFrame` has been
wrapped, the frame's drop frees the page and the caller's `free_page_frame`
frees it again. Pre-existing, not made worse by the retry work — the hoist there
closes only the arm the retry work makes reachable. The fix is to have `map`
hand the frame back on error.

**`utest_curl_e2e` and `utest_ip_e2e` need real egress.** Both open TCP to
`8.8.8.8:53` and `1.1.1.1:53`. In a sandbox whose egress policy allows only
proxied HTTPS they time out at 30 s per target and the tests fail — while
`utest_dns_resolve` passes, because QEMU's slirp DNS forwarder is a different
path. That is the environment answering, not the kernel, and nothing here
changes it. It is recorded so the next person reading a local run does not chase
it. Making them `Skipped` on a timeout is deliberately *not* done: a timeout is
what a real regression looks like too, and a test that skips itself on the
symptom of the bug it exists to catch is worse than one that fails in a sandbox.

**An unprotected window in `__spawn_kernel_io`.** The task is published to the
registry before its id is bound to the stop, so between those two lines a live
kernel-I/O thread is preserved by neither `task_registry_reset` nor
`reset_preserving`. Not currently reachable — all three spawns happen on the
BSP — but nothing enforces that.

**The guarded skip for an AP pause that proves the target took no CPU.** The
classification (`NotParking`, which is always a bug, versus `NotRunning`, which
is not evidence) is worth having and is built. Turning `NotRunning` into a
`Skipped` rather than a panic is still not done — but its prerequisite now is:
`Skipped` no longer counts into `summary.passed`, it counts into its own
`summary.skipped`, so a run that skipped everything can no longer report as a
run that passed everything. What remains before the skip is safe is gating it on
`hypervisor_present()`, so on bare metal — where a stalled heartbeat *is*
evidence — it still fails.

---

## 5. What the fixes measure

Every number here is from this environment: QEMU **TCG**, `-smp 4`, no
`/dev/kvm`. TCG is the reproduction (§1), so a green TCG run is a stronger
statement than a green KVM one — and two of the gates turned out to be
measuring the accelerator rather than the kernel, which only a run without KVM
could show.

**Before.** Four TCG runs of the unmodified tree produced eleven failures across
seven distinct tests (§1's table), including the exact failure that had
`develop` red.

**After.** Three runs of one pinned ISO — the harness now snapshots the image
and the binary before a sweep, because a concurrent rebuild silently changed
what two earlier runs measured — produced **the same two failures in all three,
and nothing else**. Neither is a flake and neither is in any family above: both
are bugs in tests written during this work, caught by running them.

  - `test_gdt_kernel_rsp0_is_a_usable_stack_top` asserted 16-byte alignment on
    the live `TSS.RSP0`. The value is `0xffffffff816ca158` — 8-aligned, and 8 is
    all the architecture needs, since the CPU pushes the interrupt frame in
    8-byte units. The 16-byte rule is a function-call boundary property. The
    assertion was simply wrong.
  - `test_tx_fire_and_forget` asserted that eight back-to-back submits advance
    the device's `tx_packets`. They advanced it by zero — and that turned out to
    be a **kernel bug**, not a bad observable: `virtio_net_transmit` submits to
    the ring and never counts, while the two other TX paths do (§3.7).

The five flake families §0 names produced no failure across the three runs.
`utest_curl_e2e` and `utest_ip_e2e` fail in this sandbox for want of egress
(§4), which is why the runs stop before the userland phase completes.

Boot's lock-class count was 71 in all three, which settles a question §3.6 left
open: it is deterministic per build, and the 71/72/73 spread seen earlier was
across *different* builds. The exact cap is the right instrument for it.

### Two gates that were measuring the host

Both were found by running them here rather than by reading them.

- **`check_quota_headroom.sh`'s `max-cycles-per-charge`** failed on the
  *unmodified* tree in all four baseline runs: the caps (3000 and 9000 cycles)
  were taken on a KVM host, and the same tree reports 20631 and 81886 under TCG.
  A gate that fails on every machine without `/dev/kvm` is a gate nobody can run
  green, which is how a documented pre-commit step stops being run.

  It is now one cap and two floors. The cap is depth 7 against depth 1 — the
  only quantity here invariant under a change of accelerator, measured at
  3.77 and 3.86 in the two pinned runs that reached the report, and at 3.69–3.99
  across the earlier runs whose logs have since been pruned. The floors are the charge
  against a same-run bare CAS, and an absolute physical bound on that CAS,
  because the first is a ratio over the second and a collapsed reference would
  satisfy it. The charge-against-CAS ratio is deliberately *not* a ceiling: a
  CAS is relatively dearer under TCG than natively, so a ceiling measured on one
  accelerator fails on the other — the very defect being removed. An earlier
  draft of this work documented that ratio as a cap in three places and
  implemented it in none; two independent reviews caught it.

  What that leaves uncaught, stated because a gate trusted for something it does
  not do is worse than no gate: a slowdown that scales the *whole* charge path
  uniformly — every depth, and a bare CAS with it — passes all three lines.
  Catching it needs an absolute cycle ceiling, and an absolute cycle ceiling
  needs a measurement on every accelerator CI might use.

  The self-test carries the cases that pin the design: the same verdict from a
  ten-times-slower host, the same rejection of a non-amortising walk from one, a
  collapsed reference rejected — and a round trip proving `--emit-allowlist`
  produces a gate file the check path then accepts, which is the property that
  makes the documented remedy for a ratchet failure actually usable. That last
  case found a real bug on its first run: the emitter wrote a `min-kinds-for`
  line unconditionally, for a phase the log need not contain.

- **`check_lockdep_headroom.sh`'s exact edge and chain caps** were the four
  ratchet failures in §0's table, on runs whose tests were green. Those are
  bands now (§3.6).

---

## 6. What the second review round found

Every fix above ships with an assertion. Reviewers were asked to build the
broken kernel each assertion names and check that it actually fails — the one
question a green suite cannot answer. Four assertions did not survive that, and
each failure was the same defect in a different place: an observable that a
correct kernel and a broken one both produce.

- **`test_quarantine_rotate_does_not_splice` passed on a rotate that spliced
  64 pages.** Rotating an empty quarantine cannot splice whatever the code does,
  and by the time the test called rotate the backlog was empty — the per-free
  paydown and every idle CPU's bottom half keep it that way. The test now parks
  a frame and drives it through both epoch closures first, and the observable is
  a counter inside `quarantine_rotate` rather than the call's return value,
  because the rotations that move a parked frame along run inside
  `quiesce`'s closure, not in the test's own call. That also makes it
  peer-proof: the claim holds for every CPU's rotations, so a peer rotating
  mid-test strengthens the assertion instead of racing it.

- **`test_reclaim_run_respects_its_bound`'s `asks == 1` was blind to the
  mutation it named.** The counter sat on the first registrant, and the first
  registrant is asked once per pass with or without `run`'s per-registrant
  budget guard; only removing the guard *and* the pass-loop break moved it. What
  the guard actually protects is everyone *behind* the first, so a registrant
  now sits there — and counts the asks that arrive with the budget already met.
  A correct `run` never asks anyone for zero pages, which makes that counter a
  witness no peer's concurrent `run` can forge: it moving is a defect wherever
  the call came from. `run`'s `want - freed` became `want.saturating_sub(freed)`
  in the same change, so the counterfactual the instrument describes is a zero
  rather than a `u32` underflow.

- **`test_rcu_drain_never_waits_for_a_grace_period` passed on a drain that spent
  2581 ms inside the masked window.** The doc claimed a dichotomy — either an
  inline wait reports a quiescent state, or it never terminates — and a wait
  bounded by its own deadline does neither. The deleted elapsed ceiling caught
  that shape, but an absolute wall-clock ceiling is the host-dependence this
  work exists to remove. The replacement is neither: a wait for a grace period
  to *complete* must reach `rcu_gp_poll`, the only operation that runs the
  completing compare-exchange, and nothing on the correct drain path calls it —
  so the expected delta is exactly zero, clock-free, and a wait that never
  reaches it cannot terminate and takes the run down loudly.

- **The COW exclusivity hoist had no falsifying test at all.** Reverting it left
  all 193 `slopos_mm::*` tests green, because the pre-fix kernel returns the
  *same* `Retry` from the single-reference arm — the bounded spin ends in
  `WouldBlock`, which maps to the same value. The cost is what differs, so the
  cost is now the observable: a per-CPU counter of spins taken in
  `vm_space_get_mut`, asserted to be zero for a fault taken with a reader
  outstanding. It reports `1000000` on the pre-fix tree.

Five comments stating guarantees their code did not have were corrected rather
than deleted where the code could be made to match, and cut back where it could
not. One test paid a 50 ms confirmation window up to 512 times to report a
single already-known frame; it now returns the rest without observing them,
which takes a failing run from 25.7 s to about 50 ms.

---

## 7. The lockdep ratchet was counting a race

`check_lockdep_headroom.sh` holds each phase's lock-class count to an **exact**
cap, on a premise the file states plainly: a class registers on the first
acquire of a declaration site, so by the end of a phase the count is a property
of the kernel, identical on every run. Twelve boots of one pinned image
disagreed — 71, 72 or 73 at boot, and whatever boot got, both test phases got
too. The three-value spread an earlier round dismissed as "across different
builds, not different runs" was across runs all along, and it failed the gate on
roughly three runs in four while the tests themselves were green.

Dumping the class table named the culprit at once:
`PriorityRunQueue.queue_lock` occupied **one, two or three slots** in the same
table. Every other class occupied exactly one.

`register_class` reserves its slot with `CLASS_COUNT.fetch_add` *before* it
knows whether it will win the link race, re-scans the bucket, and on losing
bumps `CLASS_SLOTS_LEAKED` and abandons the slot — which is correct, and is
what stops one declaration site being split across two live classes. What was
wrong is what got reported: `class_count()` is the allocation watermark, and
`kdiag_dump_lock_graph` printed it as `classes=`. On a 4-vCPU boot the per-CPU
run queues come up together, so nought to two CPUs lose that race, and the
"exact, deterministic" number moved with them.

So the reported count is now `registered_class_count()` — the watermark less
the leaked slots, which is exactly the number of distinct declaration sites —
and the line carries `leaked=` so the pool fact the raw count used to imply is
not lost. The pool-fill check keeps the watermark, because a leaked slot really
does consume one. Twelve boots after the change: `classes=71` on every one,
while `leaked=` moved between 1 and 2 across them — the racy quantity still
racing, no longer inside the number a ratchet reads.

**The recorded caps were right the whole time.** 71 / 186 / 196 is what this
tree measures once the count stops including the race, so nothing in
`scripts/gates/lockdep/` moves. A ratchet failure really was a measurement to
re-take rather than a number to raise; what needed re-taking was the
measurement's definition.
