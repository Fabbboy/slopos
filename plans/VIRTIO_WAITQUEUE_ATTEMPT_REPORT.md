# VirtIO Completion Wait Refactor Attempt (Postmortem)

## Context

While fixing `nc -l 7777 | shell` freeze behavior, we attempted to replace the current
`QueueEvent::wait_timeout_ms()` implementation in `drivers/src/virtio/mod.rs` with
a scheduler-backed `WaitQueue` wait path.

The goal was to remove CPU-local waiting behavior (`cli/sti/hlt` and AP spin fallback)
and move to a uniform blocking/wakeup model.

## What Was Attempted

1. `QueueEvent` gained an embedded `WaitQueue`.
2. `QueueEvent::signal()` started calling `wake_one()`.
3. `QueueEvent::wait_timeout_ms()` switched to:
   - `wait_event_timeout(|| self.try_consume(), timeout_ms)`
4. A follow-up mitigation was tried to wake only when `has_waiters()`.

## Result

The migration caused a deterministic kernel regression in the interrupt test harness.

- Failure type: General Protection Fault during boot/test run
- Faulting symbol: `virtio_net::VirtioNetDev::poll_rx`
- Representative RIP: `0xffffffff801c3cf6`
- Outcome: `just test` failed with interrupt test panic

Because this regressed kernel stability, the `WaitQueue` migration for `QueueEvent`
was reverted.

## Why This Is Not Merged

The attempted change mixed two event models currently used by VirtIO consumers:

- blocking completion waiters (blk request path)
- high-frequency signaling paths (net/NAPI)

The current `QueueEvent` is shared in contexts where scheduler wakeups from IRQ are
not a drop-in replacement. A broader refactor is required before adopting `WaitQueue`
for all queue events.

## Current Stable State

The tree keeps the prior stable `QueueEvent` behavior (atomic signal + timeout wait)
and all other architecture fixes remain in place.

`just build` and `just test` pass.

## Recommended Long-Term Plan

1. Split completion primitives by use case:
   - **CompletionWaitEvent** for blocking request/response waits
   - **IrqEdgeEvent** for high-rate edge notification paths (NAPI-style)
2. Migrate `virtio-blk` first to per-request/per-queue waitqueue semantics.
3. Keep `virtio-net` fast-path eventing separate until NAPI/event-loop integration is
   explicitly redesigned.
4. Add dedicated regression tests that stress IRQ wakeups and queue signaling under SMP.

This keeps correctness first and avoids unstable cross-subsystem coupling.
