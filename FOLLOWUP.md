# FOLLOWUP — open investigation: lost blocking-wait wake (boot wedge + runtime compositor freeze)

Prompt for the next agent. Read this whole file before touching code.
Branch: `fix/tty-signal-pipeline`. The compositor focus-loss bug from the
previous FOLLOWUP is FIXED and verified (in-order input dispatch); so are six
adjacent bugs found while verifying it (see the two fix commits' messages).
ONE intermittent lost-wake class remains, with two observed faces.

## TL;DR

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
