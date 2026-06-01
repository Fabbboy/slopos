# SlopOS Known Issues

Last updated: 2026-05-31

---

## Userland threads do not distribute across CPUs (thread-per-core placement)

**Status**: Open  
**Severity**: Low (no production consumer; userland is effectively single-threaded today)  
**Component**: `sched/src/scheduler.rs`, `sched/src/per_cpu.rs`

### Description

Userland OS threads never run on application processors (APs) even when given a non-zero CPU
affinity — they stay on the BSP (CPU 0). The kernel IS fully SMP (per-CPU runqueues, AP schedulers
online, and APs *can* run userland: CR3 / FS_BASE / GS / IST / TSS / SYSCALL are all set up per-AP),
and placement honors affinity at task *creation* and at *wake*. The gap is two-fold; the second half
is the hard one:

1. **No re-placement of a runnable thread.** Affinity is not consulted at the post-slice re-enqueue
   (`run_ready_task_from_idle` re-queues to the current CPU) nor by `set_cpu_affinity` (it only stamps
   the mask). A CPU-bound thread pinned to an AP re-queues to CPU 0 forever. This half is a small fix.
2. **No cross-core re-dispatch of a `ring_enter`-parked thread woken cross-core.** The deeper blocker:
   once a worker parks in `ring_enter` on a non-zero CPU and is woken by a cross-core fd write (the
   slopfut cross-core channel's wakeup self-pipe), it is not re-dispatched on its CPU and the
   round-trip hangs. A strict single-CPU non-zero pin can also dead-end a wake (`select_target_cpu`
   returns `None` when no permitted CPU is momentarily schedulable → the wake is silently dropped).

Discovered implementing Phase-6 Tier B. A placement-only fix made all 4 workers migrate to distinct
CPUs, but the cross-core round-trip then hung on #2, so the fix was reverted. The Tier-B
per-thread-reactor + cross-core-channel infrastructure works fully **co-located** (all reactors on
CPU 0); `percore_reactor_test` validates that.

### Impact

Thread-per-core is logically complete (N independent reactors + a `Send` cross-core channel) but not
physically distributed: all reactors time-slice on CPU 0. No current consumer needs distribution
(production userland is single-threaded), so impact is low today.

### Fix (dedicated effort — both halves needed together)

1. Re-enqueue honors affinity: route an affinity-disallowed re-enqueue through `schedule_task` (after
   `on_cpu` clears).
2. `set_cpu_affinity` re-places a Ready task on a now-disallowed CPU.
3. `select_target_cpu`: affinity-permitted online-CPU fallback instead of `None`.
4. **Cross-core ring-wakeup re-dispatch** (load-bearing): a `ring_enter`-blocked task woken via a
   cross-core fd write must be re-dispatched on an affinity-permitted CPU and its reactor roused
   there. This is why the naive placement fix is insufficient alone.

### Related Files

- `sched/src/scheduler.rs` — `run_ready_task_from_idle`, `schedule_task`, `unblock_task`
- `sched/src/per_cpu.rs` — `select_target_cpu`, `affinity_allows_cpu`, `enqueue_local`
- `core/src/syscall/process_handlers.rs` — `syscall_set_cpu_affinity`
- `slopos-rt/src/slopfut/{reactor,cross_core}.rs` — the cross-core wakeup-fd path
- `userland/src/bin/tests/percore_reactor_test.rs` — the co-located validation

---

## SlopRing: zero-copy is single-direct-copy (not DMA) + OP_CONNECT missing

**Status**: Open  
**Severity**: Low (perf frontier + one functional gap — registered/provided buffers now work)  
**Component**: `ring/`, `net/`, `drivers/src/virtio_net.rs`, `abi/src/ring.rs`

### Done (no longer stubbed)

**Registered fixed buffers + provided buffer rings landed** (full io_uring-parity buffer
surface). `ring_register` implements `RING_REGISTER_BUFFERS` / `RING_UNREGISTER_BUFFERS`
(fixed buffers, `Sqe.buf_index`) and `RING_REGISTER_PBUF_RING` / `RING_UNREGISTER_PBUF_RING`
(provided rings, `Sqe.buf_group` + `SLOPRING_SQE_BUFFER_SELECT`, `bid` reported in
`SLOPRING_CQE_F_BUFFER`); `SLOPRING_FEAT_REG_BUFFERS` is advertised. Backed by a sound
**`mm/src/pinned_user_buffer.rs::PinnedUserBuffer`** (pins anonymous user pages via an
`AnonymousMeta` refcount, accessed volatilely — `ring/` stays `#![forbid(unsafe_code)]`,
zero new `unsafe`). The per-op 4 KiB `KVec` allocation, the per-op page-table walk, and the
SMAP user-copy are eliminated on the selected path; `buf_group == 0` + no fixed flag keeps the
inline path byte-for-byte. UAF-safe check-out/check-in reclaim mirrors the reactor's
orphan-slot reaping; the index/cursor bounds are machine-checked in
`verification/proofs/ring_bufpool.rs`.

### Remaining frontier

1. **Single-direct-copy (one kernel staging hop remains).** The selected path still stages the
   payload through the ring's reusable kernel scratch (one volatile copy) because the net
   primitives (`socket_send`/`unix_send`/`socket_recv`) require a kernel `&[u8]` and §5.3/AD-3
   forbid a kernel reference over the user-writable pinned pages. Eliminating that hop — a true
   single copy straight between the pinned pages and the TCP/UDP/unix socket buffer — needs a
   volatile `VmReader`/`VmWriter` (in `slopos-ostd`) threaded through the net stack's
   buffer-fill paths (`tcp/buffer.rs::enqueue`, the udp/unix equivalents, and the recv side). It
   is a wide, careful change to the core net data path; the dominant per-op costs (alloc,
   page-walk, SMAP) are *already* gone, so this is a copy-count refinement, not a correctness
   gap.
2. **Phase D — true NIC-DMA zero-copy.** `PinnedUserBuffer::io_slices()` already yields coalesced
   `(paddr, len)` runs; virtio TX (`drivers/src/virtio_net.rs`) supports scatter-gather, so a TX
   descriptor chain `[header] + [pinned user page paddrs]` would let the NIC DMA the payload
   directly (0 CPU copies on send). Needs a zero-copy-send notification CQE
   (`SLOPRING_CQE_F_NOTIF`, pin held until the TX used-ring reclaim) and is sound only for
   UDP/raw and `MSG_ZEROCOPY`-style TCP (retransmit ownership).
3. **`OP_CONNECT`** — not implemented (`OP_MAX == OP_CLOSE`). `connect(2)` is still a synchronous
   syscall. Low impact (one-shot). Fix: a thin probe adapter over the non-blocking sync connect,
   bump `OP_MAX`, expose a slopfut `connect` future.

### Related Files

- `mm/src/pinned_user_buffer.rs` — the sound pinned-user-buffer primitive (volatile copy only)
- `ring/src/buffers.rs` — the per-ring registered/provided buffer registry
- `ring/src/net_glue.rs` — `*_inline` (byte-for-byte) vs `*_pinned` paths; the staging hop to remove
- `net/src/tcp/buffer.rs`, `net/src/socket.rs`, `net/src/unix_socket/mod.rs` — the volatile-reader
  integration points for single-direct-copy
- `drivers/src/virtio_net.rs` — scatter-gather TX for Phase-D DMA
- `abi/src/ring.rs` — `OP_MAX` (for `OP_CONNECT`); `SLOPRING_CQE_F_NOTIF` (to add for Phase D)

---

## Performance: Compositor Frame Rate During Task Termination

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

When a task terminates, `pause_all_aps()` is called which blocks all AP scheduler loops. While this is necessary for safe task cleanup, it can cause brief stalls in compositor frame rendering if the compositor happens to be scheduled on an AP.

### Current Behavior

1. Task calls `task_terminate()`
2. `pause_all_aps()` sets `AP_PAUSED = true` and waits for APs to stop executing
3. `release_task_dependents()` unblocks waiting tasks
4. `resume_all_aps()` sets `AP_PAUSED = false` and sends wake IPIs

During steps 2-3, any task on an AP (including compositor) is paused.

### Impact

- Brief frame drops (1-2 frames) during task termination
- More noticeable with frequent task spawning/termination

### Potential Optimizations

1. **Fine-grained locking**: Instead of pausing all APs, use per-task locks
2. **RCU-style cleanup**: Defer task cleanup to a dedicated kernel thread
3. **Lock-free dependent release**: Use atomic operations instead of global pause

### Related Files

- `core/src/scheduler/task.rs` - `task_terminate()`
- `core/src/scheduler/per_cpu.rs` - `pause_all_aps()`, `resume_all_aps()`

---

## Performance: Scheduler Lock Contention

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

The scheduler uses a global `SCHEDULER` mutex that can cause contention when multiple CPUs try to schedule tasks simultaneously.

### Current Architecture

```
SCHEDULER (global IrqMutex)
├── ready_queues[4]     // Priority-based queues
├── current_task
├── idle_task
└── various counters

CPU_SCHEDULERS[MAX_CPUS] (per-CPU)
├── ready_queues[4]     // Local priority queues
├── current_task_atomic
└── queue_lock (per-CPU mutex)
```

### Contention Points

1. `schedule()` calls `with_scheduler()` which locks global mutex
2. `schedule_task()` may fall back to global queue if per-CPU enqueue fails
3. `select_next_task()` checks both per-CPU and global queues

### Impact

- Minor latency spikes under high task churn
- Not significant with current workloads (compositor + shell)
- Would become more noticeable with many concurrent tasks

### Potential Optimizations

1. **Fully per-CPU scheduling**: Eliminate global ready queue entirely
2. **Lock-free queues**: Use compare-and-swap for enqueue/dequeue
3. **Batch operations**: Coalesce multiple schedule operations

### Related Files

- `core/src/scheduler/scheduler.rs` - `SCHEDULER`, `with_scheduler()`
- `core/src/scheduler/per_cpu.rs` - `CPU_SCHEDULERS`

---

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `lib/src/pcr.rs` for architecture details.
