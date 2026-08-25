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
