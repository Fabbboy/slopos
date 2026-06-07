# FOLLOWUP — open investigations on `fix/tty-signal-pipeline`

Prompt for the next agent. Read this whole file before touching code.
The compositor focus-loss bug from the previous FOLLOWUP is FIXED; so are
the kernel ISR/ICR/spawn bugs, and the **resize freeze** (TLB-shootdown
wedge — fixed by IPI re-send, commit daa9652d). Two items remain.

## RESOLVED this session: resize freeze (TLB shootdown wedge)

The "resizing the terminal freezes all of SlopOS" bug is FIXED (commit
daa9652d) and verified: 60 aggressive grow/shrink cycles stay live where
the unfixed build froze within a handful. `just debug-bt` on a frozen VM
showed CPU0 wedged in `tlb::wait_for_acks` on the munmap path
(`process_vm_munmap → luf::queue_unmap → drain_all → tlb::flush_all`) while
peers sat idle-HLT, never acking. Root: a single shootdown IPI is not a
trustworthy delivery (ICR-busy past `wait_icr_idle`'s cap; AB-BA lock cycle
leaving a target IPI-unreachable). Fix: both shootdown waits (TLB boolean
ack + LUF drain, the latter converted counter→per-CPU bitmask) re-send the
IPI on timeout, bounded, idempotent.

**STRUCTURAL FOLLOW-UP (do this to retire the safety net):** the re-send is
a robust net but it papers over an AB-BA deadlock — `process_vm_munmap`
holds the cli'd `PROCESS_VMS` SpinLock across the shootdown, and a peer
spinning cli'd to acquire that same lock cannot service the initiator's
shootdown IPI. Dissolve it at the source, exactly as `DRAIN_LOCK` was
already changed SpinLock→PreemptMutex (mm/src/mmu/luf.rs:205): either make
the `PROCESS_VMS` wait IRQ-serviceable, or collect the unmap ranges, DROP
the lock, then flush (the "shootdown out from under the VM lock" pattern).
Then `wait_for_acks` always completes and the give-up path is dead code.
Confirm by stress-resizing with the give-up `klog_warn` as a tripwire — it
must never fire.

## TL;DR (remaining lost-wake bug)

A task blocked in a kernel wait intermittently never receives its wake. Two
reproduced manifestations, almost certainly one root cause:

1. **Boot wedge** (~1 boot in 3–5, QEMU q35, 4 CPUs, KVM): the terminal
   window never appears. The terminal is permanently **Blocked** inside its
   `spawn("/bin/shell")` — exec's ELF load waiting on a scheduler-backed
   virtio-blk read whose completion never wakes it.
2. **Runtime compositor freeze** (several minutes of heavy interaction —
   resize storms + `yes` floods): the compositor's single-threaded `block_on`
   parks in the SlopRing harvest wait and never wakes. The system-bar clock
   freezes, every keystroke falls through to the kernel-console fallback
   (echo only), the screen keeps the last rendered frame. The rest of the
   system stays healthy. Reproduced twice (~16 s and ~26 s of interaction
   into a session).

In both cases the stranded task is `Blocked` with block reason `Sleep` and
is ABSENT from the kernel sleep queue. Note `WaitQueue::wait_event`-style
waits set reason `Sleep` without a sleep-queue entry, so the strand is most
likely a lost **WaitQueue/event wake** (or a harvest fd-registration wake),
not a sleep-timer loss — pin down which wait primitive the victim is in
first (saved-context RIP of the blocked task is the fastest discriminator).
This is (at least one face of) the long-standing "boot roulette" flakiness.

## Evidence captured live (instrumented runs)

- Failing-boot task dump: `terminal st=Blocked blk=Sleep`, the half-built
  `shell` task parked `Blocked` (correct, new exec discipline), every
  runqueue empty, all four CPUs idle-HLT with IF=1, LAPIC timers periodic
  and ticking, no pending IRQs in any LAPIC IRR/ISR.
- The terminal is Blocked with **no sleep-queue entry** (a periodic sleeper
  dump showed only the genuinely sleeping tasks). Blocked + reason `Sleep` +
  absent from the sleep queue means its timeout entry was POPPED but the
  task was never unblocked — the wake fell on the floor.
- The protocol handshake preceding the wedge is complete and healthy
  (accept → greet → CreateSurface → Attach all processed); the terminal then
  enters PTY/shell spawn and never returns to commit its first frame.

## Freshest evidence (final stripped build, first boot-cycle attempt)

A wedged boot died EARLIER than the terminal case: screen fully black
(compositor's exec never completed), all CPUs idle-HLT, and the serial log
ends with `SCHED: rescuing stranded READY task 7` (ext2-flush) repeating
forever — the disk-flush kthread cycles wake → stranded-unlinked → rescue
every sweep. So a *periodically sleeping kthread's wake-side enqueue is
being lost on every cycle* (the rescue sweep is acting as its de-facto
waker), and the disk path it serves never makes progress. Earlier
instrumented boots showed the same lines for init/compositor
("rescuing stranded READY task 8/9" hundreds of times) — previously
misread as benign churn. Start here: trace ONE wake cycle of ext2-flush
(wake_due_sleepers → wake_sleeping_task CAS → schedule_task → enqueue
local-vs-remote-inbox → drain) and find which leg drops the enqueue while
leaving the task Ready. The earlier DBGQ counters showed remote-inbox
drops with `status Running/Blocked` — re-add those plus a counter on
schedule_task's target selection.

Also note: the interactive resize freeze (wait_for_acks spin) did NOT
reproduce on the final build — 70 slow-drag iterations / 7400 broadcast
shootdowns clean, with per-CPU queued/sent/handled counters internally
consistent. A user-captured freeze with that exact backtrace almost
certainly came from a pre-ICR-fix ISO; if it ever recurs on a current
build, re-add the four DBGI counters in mm/src/tlb.rs
(queue_request_for_cpu / send_shootdown_ipi_to_cpu / handle_shootdown_ipi)
and read them post-mortem via QMP gva2gpa+xp.

## Post-fix user retest (fresh build, confirmed not stale)

- "SCHED: rescuing stranded READY task [..]" spams CONSTANTLY even on
  healthy interactive boots. The wake-side enqueue loss is therefore
  ROUTINE, not rare — the rescue sweep is acting as the system's de-facto
  waker, and any wait without that backstop (the compositor's parked
  block_on, exec's disk-read CompletionEvent) wedges permanently when its
  wake is the one that gets lost.
  OPEN QUESTION: does c068398a (pre-branch) spam the same way? Boot it and
  grep serial for "rescuing" to learn whether this branch's sched changes
  (idle-loop IRQ bracketing, ISR resched gating) widened the loss window
  or merely made pre-existing loss visible. (c068398a still carries the
  preempt panics, so expect some boots to die.)
- The interactive freeze recurred on a FRESH build while resizing the
  terminal SMALLER — so the earlier "pre-ICR-fix ISO" attribution is in
  doubt. Discriminate with `just debug-bt` on the frozen state:
  * a CPU spinning in `wait_for_acks` → the shootdown-ack loss is NOT
    fully closed by the ICR fix; re-add the DBGI counters (below) and
    read queued/sent/handled per CPU post-mortem.
  * all CPUs halted, nothing spinning → the lost-wake face (victim task
    Blocked; get its saved-context RIP via task_switch_ctx_rip_rsp dump).
  Note the scripted 70-iteration drag stress did NOT reproduce either
  face on the same build — the human drag pattern (or shrink-heavy
  resizes) still differs from the harness in some way that matters;
  consider replaying shrink-dominated drags with varied speeds.

## Prime suspects, in order

Audited already (look correct in isolation): `unblock_task` cancels the
sleep entry only AFTER winning its Blocked→Ready CAS;
`block_current_task_with_timeout` orders CAS-Running→Blocked before
`SLEEP_QUEUE.upsert`; `wake_due_sleepers`'s drop paths only fire when a
competing waker already won. So look one layer down:

1. `slopos_ostd::sync::wait_queue` (`enqueue_current` + heap `WaitNode` +
   the "lock-pair barrier"): the IRQ-side wake (virtio-blk completion,
   ring fd readiness) racing the waiter's commit-to-Blocked. The WaitMux
   migration memory note says this code is still the old enqueue_current
   design. A wake delivered between the waiter's predicate re-check and its
   deschedule commit may be consumed without a retry.
2. The SlopRing harvest registration (`ring/src/enter.rs register_fds` +
   `file_poll_track_registrations` → wake delivery): SLOPRING §7.1
   register-then-recheck is supposed to close the lost-wakeup window — but
   the compositor freeze parks exactly in `harvest`'s
   `block_current_task_with_timeout`, so either the fd wake OR the timeout
   wake (16 ms frame timer → OP_TIMEOUT deadline → sleep_budget) was lost.
   Note for the frame-timer-only case the ONLY wake is the sleep deadline.
3. `commit_blocked_deschedule`'s rc==false "wake consumed" arm interacting
   with a wake that arrives between the SLEEP_QUEUE upsert and the
   deschedule commit — confirm the consumed wake cannot be one the task
   still needs after it re-blocks.

## Repro harness

```bash
BOOT_CMDLINE="tests=off roulette=skip" just _iso-notests
/tmp/slop-bootcycle.sh 15    # boots repeatedly; stops on the first failure
```

`/tmp/slop-bootcycle.sh` (recreate if gone): boot QEMU headless
(`-display vnc=:77`, serial to `/tmp/slop-serial.log`, QMP socket at
`/tmp/slop-qmp.sock` — full invocation in `/tmp/slop-boot.sh` from this
session, or copy the one in the test harness scripts), wait up to 25 s for
`DBGW: window_count 0 -> 1` on serial — with tracing stripped, instead probe
a screenshot pixel or grep for the shell banner — and report
WINDOW/NOWINDOW/PANIC per boot.

Diagnostics that made this tractable (all stripped before commit — re-add
locally as needed):
- A periodic task dump in `scheduler_timer_tick` (every ~8000 global ticks):
  task id/name/status/block-reason (+ saved context RIP for blocked tasks)
  via `task_iterate_active`, plus per-CPU `total_ready_count` and the active
  sleep-queue entries with their deadlines vs now.
- QMP monitor forensics on the wedged VM: `info registers -a` (per-CPU
  RIP/IF), `info lapic <n>` (IRR/ISR/timer), `gva2gpa` + `xp` to read kernel
  statics in the live image.

## What is already fixed on this branch (do not re-hunt)

- Compositor focus loss / key drops (in-order input dispatch; focus
  re-acquire; dock geometry hit-test; accept re-arm + per-frame accept sweep
  and per-frame `process_requests()` parse — all four were independently
  reproduced failure modes).
- Kernel preempt_count corruption panics (deferred-reschedule callback firing
  inside ISRs; idle-loop dispatch with IRQs enabled).
- xAPIC ICR_HIGH/ICR_LOW write pair torn by interrupting IPI senders —
  redirected IPIs, permanently lost TLB-shootdown acks (terminal spinning
  forever in `wait_for_acks` after ~10 resize cycles).
- Stranded-READY fresh tasks: rescue sweep now rescues never-run tasks after
  3 consecutive stranded observations; exec keeps the task Blocked during
  the ELF load (it was visible Ready-half-built for tens of ms and a rescue
  dispatch jumped to an unmapped entry point).
- `input_poll_batch` ENOMEM (per-call heap scratch) panicking the compositor
  via errno-as-huge-count; now chunked stack scratch + clamped userland
  wrapper.
- Right-Ctrl/Right-Alt: E0-prefixed modifiers were misrouted AND ate the next
  scancode (E0 latch consumed too late in `handle_scancode`).
- `yes`/`seq` misreporting Ctrl-C as write failure (exit 0, interrupt flag
  left pending).

## Verification expectations for the fix

- `just test` green (baseline 2545).
- `/tmp/slop-bootcycle.sh 20` → 20/20 WINDOW.
- The interactive repro from the previous FOLLOWUP (resize → click → `yes` →
  single Ctrl-C), 10+ alternating grow/shrink iterations, with the
  system-bar clock confirmed still advancing at the end (the freeze face
  fails silently: a dead compositor logs no key drops — check the clock or
  type a key and verify it renders).
