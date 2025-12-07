# Scheduler and Tasking TODO

## Privilege Separation
- [ ] Run shell (and other user apps) as `TASK_FLAG_USER_MODE` tasks with their own process VM.
- [ ] Move roulette/user apps onto int 0x80 syscalls (yield/exit plus minimal I/O) and remove direct kernel calls.
- [ ] Add copyin/copyout guards for syscall buffers so ring3 cannot scribble on kernel memory.
- [ ] Default non-kernel tasks to ring3 unless explicitly marked `TASK_FLAG_KERNEL_MODE`.

Note: int 0x80 is the user gateway (SYSCALL_YIELD=0, SYSCALL_EXIT=1)

## Scheduling Enhancements
- [ ] Calibrate/use LAPIC timer for preemption (PIT-based preemption exists).

## Async Coordination
- [ ] Extend join/wait primitives with timeout and cancellation support.
- [ ] Provide a lightweight async completion primitive for cross-task signaling.

_Pending:_ A detailed execution plan will be pushed to elaborate on each item before implementation starts.
