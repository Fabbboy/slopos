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

## SlopRing: NIC-DMA zero-copy send (Phase D) + OP_CONNECT missing

**Status**: Open  
**Severity**: Low (perf frontier + one functional gap — single-direct-copy + registered/provided buffers now work)  
**Component**: `ring/`, `net/`, `drivers/src/virtio_net.rs`, `abi/src/ring.rs`

### Done (no longer stubbed)

**Registered fixed buffers + provided buffer rings** (full io_uring-parity buffer surface) and,
now, **single-direct-copy** on every selected path. `ring_register` implements
`RING_REGISTER_BUFFERS` / `RING_UNREGISTER_BUFFERS` (fixed buffers, `Sqe.buf_index`) and
`RING_REGISTER_PBUF_RING` / `RING_UNREGISTER_PBUF_RING` (provided rings, `Sqe.buf_group` +
`SLOPRING_SQE_BUFFER_SELECT`, `bid` in `SLOPRING_CQE_F_BUFFER`). Backed by a sound
**`mm/src/pinned_user_buffer.rs::PinnedUserBuffer`** (pins anonymous user pages via an
`AnonymousMeta` refcount, accessed volatilely — `ring/`/`net/`/`mm/`/`drivers/` all stay
`#![forbid(unsafe_code)]`, the only `unsafe` is `slopos-ostd`'s existing volatile ops).

**Single-direct-copy landed (was item 1).** The per-ring 4 KiB kernel scratch is **gone**. A
volatile `VmReader`/`VmWriter` cursor (`slopos-ostd/src/mm/vmcursor.rs`, built on the existing
safe `UFrame::copy_{out,in}_volatile` — zero new `unsafe`) walks the pinned `UFrame` chain and
is threaded through the net data path (`RingBuffer::write_from`/`read_into`,
`tcp::send_from`/`recv_into`, `udp_sendto_from`, `socket_send_pinned`/`socket_recv_pinned`,
`unix_send_from`/`unix_recvmsg_into`, `PacketBuf::append_from`). The selected fixed/provided
paths now do **exactly one** volatile copy straight between the pinned pages and the
TCP/UDP/unix socket buffer (no staging hop):

| Path | copies before | copies now |
|---|---|---|
| UDP send (`pin → PacketBuf`) | 2 | **1** |
| UDP recv (`recv_queue payload → pin`) | 2 | **1** |
| TCP send (`pin → 32 KiB send ring`) | 2 | **1** |
| TCP recv (`recv ring → pin`) | 2 | **1** |
| unix send/recv (`pin ↔ KVecDeque`, byte-wise) | 2 | **1** |

(ICMP fixed-buffer send mirrors UDP — `socket_send_pinned` → `send_echo_request_from` →
`PacketBuf::append_from`, also 2 → 1.)

These counts are the **socket-layer** copy (pin ↔ socket buffer) — exactly the staging hop
Goal 1 names. On the **send** side there is still one *further*, pre-existing copy that the
inline path also pays: the virtio driver copies the socket buffer (`PacketBuf` / the TCP send
ring's segment) into a kernel DMA page before the NIC reads it (`virtio_net::tx` →
`tx_page.write_slice`). That driver copy is **not** eliminated here — eliminating it (so the
NIC DMAs the payload straight from the pinned pages, 0 CPU copies) is the Phase-D work below.

`buf_group == 0` + no fixed flag keeps the inline (`*_nonblock`) path byte-for-byte. The
cursor's offset/advance/no-cross-frame-slice bounds are machine-checked in
`verification/proofs/vmcursor.rs` (11 obligations) and the cursor is KernMiri-clean.

### Done (continued) — `OP_SEND_ZC` opcode + two-CQE notification

**`OP_SEND_ZC` landed** (= 12, `OP_MAX` bumped) with the full io_uring `SEND_ZC` ABI: the
two-CQE protocol via `SLOPRING_CQE_F_NOTIF` (= `1 << 3`). A successful zero-copy send posts a
result CQE carrying `SLOPRING_CQE_F_MORE` ("notification to follow") and then a terminal CQE
carrying `SLOPRING_CQE_F_NOTIF` (the registered fixed buffer is reusable). It requires the
fixed-buffer flag (must name its pinned data); the inline/provided selections are rejected
`-EINVAL`. **Backend today is the single-direct-copy leaf** (`socket_send_pinned` /
`unix_send_from` — one volatile copy from the pinned pages into the socket buffer), so the
buffer is reusable the instant the copy returns and the notification follows immediately —
exactly io_uring's `COPIED` fallback. **Tested end-to-end, no enabler:** a userland connected-UDP
`OP_SEND_ZC` reaps both CQEs (`udp_send_zc_two_cqe`) and the dispatch rejection is asserted
(`send_zc_requires_fixed_buffer`); the wire flags round-trip in `cqe_notif_bit_pack`.

### Remaining frontier

1. **Phase D — true NIC-DMA 0-CPU-copy send (the perf optimization on top of `OP_SEND_ZC`).**
   The opcode + two-CQE protocol above are live; the remaining work removes the payload copies
   entirely — today the payload is copied pinned-pages → socket buffer (Goal 1's single copy)
   and then socket buffer → DMA page (the driver's `tx_page.write_slice`); Phase D makes the NIC
   DMA the payload straight from the pinned pages (the SG chain carries the headers in a small
   kernel page and points the remaining descriptors at the pinned-page paddrs), so **0** CPU
   copies of the payload. **Groundwork landed + unit-tested:** `slopos-ostd` `TxReclaimToken`
   (the lock-free driver→ring reclaim signal that fits the harvest/re-poll model — drivers
   cannot depend on `ring` nor post a CQE from NAPI context, host tests) and
   `drivers/src/virtio_net.rs::build_tx_chain` (the SG descriptor chain
   `[header] --F_NEXT--> pinned io_slices() runs`, stest `test_build_tx_chain_links_runs`).
   The driver requests `VIRTIO_NET_F_CSUM` as an optional feature, so when the device offers it
   the payload checksum can be offloaded rather than CPU-computed. **Still to wire:**
   `NetDevice::tx_zerocopy` (+ `DeviceHandle` wrapper) + the live virtio SG submit + a
   `TxSlot` that holds the header page/token until `virtnet_clean_tx` reclaims it and signals the
   token; switch `send_zc_fixed` from the copy leaf to a **deferred** notif (the harvest posts
   `F_NOTIF` once the token flips, then `check_in_fixed`) driven by a header-only
   `udp_sendto_zerocopy` (csum-offload pseudo-header seed). Sound for **UDP/raw** (single
   transmit). **TCP `MSG_ZEROCOPY`** is the deepest follow-up — the send path must hold the pinned
   pages until ACK (retransmit ownership) and read from the pin on retransmit, a send-queue
   rework (`Inline | Zerocopy` chunks) with its own proof.

   > **Testability note:** the deferred-notif DMA path *is* in-harness testable — QEMU's SLIRP
   > backend returns TX descriptors to the used ring (so the reclaim → `F_NOTIF` fires) and a
   > `-object filter-dump,queue=tx` pcap would let a host-side `memcmp` confirm the payload bytes
   > were DMA'd from the pinned pages. The pcap host-assertion (a Go/tshark enabler in
   > `tools/run_tests`) is the only piece deferred for cost; the two-CQE reclaim itself needs no
   > enabler.

2. **`OP_CONNECT`** — not implemented. `connect(2)` is still a synchronous syscall. Low impact
   (one-shot). Fix: a thin probe adapter over the non-blocking sync connect, bump `OP_MAX`,
   expose a slopfut `connect` future.

### Related Files

- `slopos-ostd/src/mm/vmcursor.rs` — the volatile `VmReader`/`VmWriter` cursor (single copy)
- `slopos-ostd/src/tx_reclaim.rs` — `TxReclaimToken` (driver→ring reclaim signal, Phase D)
- `mm/src/pinned_user_buffer.rs` — the sound pinned-user-buffer primitive (`reader`/`writer`)
- `ring/src/buffers.rs` — registry: `fixed_reader`/`fixed_writer`/`provided_pin` (scratch removed)
- `ring/src/net_glue.rs` — `*_nonblock` inline (byte-for-byte) vs the cursor-threaded fixed/provided paths
- `net/src/socket.rs`, `net/src/tcp/{buffer,mod}.rs`, `net/src/udp.rs`, `net/src/unix_socket/` — `*_pinned`/`*_from`/`*_into` leaves
- `drivers/src/virtio_net.rs` — `build_tx_chain` SG builder; live SG submit is Phase-D follow-up
- `abi/src/ring.rs` — `OP_SEND_ZC`, `SLOPRING_CQE_F_NOTIF` (landed); `OP_MAX` (for `OP_CONNECT`)

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
