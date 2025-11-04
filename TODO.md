# Scheduler and Tasking TODO

## Privilege Separation
- [ ] Route non-kernel tasks through a user-mode entry stub before handoff.
- [ ] Implement safe elevation/drop helpers so threads can transition between rings.
- [ ] Define kernel ABI for returning from user space (interrupt frame, syscall gate).

## Scheduling Enhancements
- [ ] Add preemption hooks once LAPIC timer calibration lands.
- [ ] Support per-task time slices and basic load accounting across kthreads.

## Async Coordination
- [ ] Extend join/wait primitives with timeout and cancellation support.
- [ ] Provide a lightweight async completion primitive for cross-task signaling.

## Hardware interrupts
- [ ] ABSOLUTE PRIORITY: Add Hardware Interrupts instead of polling for devices.

_Pending:_ A detailed execution plan will be pushed to elaborate on each item before implementation starts.
