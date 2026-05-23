---
name: SlopOS Net/Scheduler Refactor — Phases 3, 4, 5
description: Lock-free TCP demux (Phase 3) + typed KernelEvent substrate (Phase 4) + Rust-typed XDP (Phase 5). Continuation of the Phase 1+2 RX/scheduler rip-and-replace.
status: ready (Phase 1+2 + unix_socket atomicity fix shipped)
parent_plan: (not committed) — original five-phase plan lived at `~/.claude/plans/fully-implleme-tlong-term-refactored-rocket.md`
working_directory: /home/lon60/repos/slopos
---

# SlopOS Net/Scheduler Refactor — Phases 3, 4, 5

> **Status:** Phase 1 (threaded NAPI + scheduler invariants + tickless idle + `KernelIoToken`) shipped. Phase 2 (state-machine retirement + lost-wakeup recheck via `Virtqueue::has_pending`) shipped. The single-critical-section `unix_sendmsg` atomicity fix for SCM_RIGHTS shipped on top (made window rendering work again).

> **Target:** finish the network/scheduler rip-and-replace by retiring the last per-packet RX serialisation point (Phase 3), unifying the 33+ ad-hoc wait queues into one typed event substrate (Phase 4), and replacing BPF-style packet filtering with safe-Rust filters monomorphised by `rustc` (Phase 5).

> **Headline KPIs:**
> - **Phase 3:** TCP RX read path acquires zero SpinLocks under contention. `tcp::input` runs entirely under `NET_EPOCH.enter()` + `RcuCell::load`.
> - **Phase 4:** zero remaining `static ... [WaitQueue; CAP]` arrays outside `slopos-ostd/src/sync/`. Every wake site goes through `bus.publish(KernelEvent::…)`. Producer/consumer mismatches become `rustc` errors instead of silent `void *key` drift.
> - **Phase 5:** an `#[xdp_filter]` Rust function can drop / pass / mirror traffic at the NAPI hot path with zero `unsafe`, zero JIT, and zero BPF verifier — `rustc` + `forbid(unsafe_code)` is the verifier.

---

## 0. What's already shipped (read this before starting Phase 3)

Future agents must understand the shape of the current tree, because Phases 3-5 build on it directly.

### Phase 1 — threaded NAPI + scheduler invariants

- **`TaskPriority::KernelIo = 1`** (between `High` and `Normal`), reserved for kernel I/O kthreads, rejected at the user spawn syscall. See `abi/src/task.rs::TaskPriority`, `core/src/syscall/process_handlers.rs::syscall_spawn_path`.
- **`KernelIoToken`** — compile-time witness; the only way to create one is the `spawn_kernel_io!` macro. `yield_with_deadline(&token, Deadline)` is the sole sleep API at this priority — `Immediate`, `AtMs(u32)`, or `Indefinite`. See `slopos-ostd/src/sync/kernel_io_task.rs`.
- **Preempt-on-enqueue.** `sched/src/scheduler.rs::schedule_task` and `schedule_new_task`: when a higher-priority task lands on the local CPU's runqueue, `scheduler_request_reschedule(InterruptWake)` sets the pending flag; trap-exit dispatches it. Matches Linux `try_to_wake_up → check_preempt_curr → resched_curr`. The actual context switch is deferred (`PreemptGuard` re-entry / trap-exit boundary), so callers can hold critical sections through a wake.
- **Tickless idle.** `sched/src/scheduler.rs::arm_tickless_idle_if_due` is called from every idle-HLT site (`sched/src/runtime.rs:226, :499, :521`). It peeks `SleepQueue::earliest_deadline` and, if the deadline is inside the current 10 ms periodic tick window, programs LAPIC one-shot via `platform::timer_program_next_wakeup_ms`. The next `scheduler_timer_tick` restores periodic mode via `restore_periodic_if_armed`.
- **`NapiWaker` primitive.** `AtomicBool armed + WaitQueue wq`. IRQ-safe `arm_and_wake` from the NIC IRQ handler; consumer parks indefinitely on `wait()`. See `net/src/napi_waker.rs`.
- **virtio-net kthreads.** Two `KernelIo` kthreads: `netpoll` (drains RX, runs ingress) and `net-timer` (50 ms ARP-age/retransmit/delayed-ACK cadence). Spawned via `spawn_kernel_io!`. See `drivers/src/virtio_net.rs::napi_thread_entry` and `net_timer_thread_entry`.
- **Build gate.** `scripts/check_wait_predicate_purity.sh` forbids `napi::kick`, `napi::wake_napi`, `force_napi_poll`, `sleep_current_task_ms` inside `wait_event{,_timeout,_until}` closures. Enforced from `just check-framekernel`.

### Phase 2 — kill polling shape entirely

- **`NapiContext` state machine retired.** The `Idle/Scheduled/Polling` CAS in `net/src/napi.rs` is gone; only `budget: u32 + processed: AtomicU32` remain for instrumentation. Single IRQ producer + single kthread consumer via `NapiWaker` makes the CAS structurally redundant.
- **`Virtqueue::has_pending()`** added (`drivers/src/virtio/queue.rs`) — lock-free peek of `used.idx` vs `last_used_idx`. The NAPI kthread calls it after every burst; if the IRQ raced the re-park, the waker is re-armed and the next `wait()` returns immediately. Closes the lost-wakeup window structurally (Linux NAPI's `napi_complete_done` shape).
- **Synchronous `napi::kick` API retired.** `register_kick`/`kick` deleted from production paths. Test-only callers go through `(NetDriverServices.virtnet_force_napi_poll)()`.

### Bonus — `unix_sendmsg` atomicity fix (this turn)

- **The bug:** `unix_sendmsg` used to write data in one critical section then push fds in a second. With Phase 1.2 preempt-on-enqueue, the peer would wake from `unix_recv` between the two and drain the data without seeing the fds — so SCM_RIGHTS-bearing messages like Wayland-style `SurfaceAttach` decoded with `buffer_fd: None`, breaking window rendering.
- **The fix.** `unix_sendmsg` is now one critical section: validate, capacity-check both publish targets, push fds first, write data, drop lock, `wake_all` once. Matches Linux `unix_scm_to_skb → __skb_queue_tail → sk_data_ready` and FreeBSD `unp_internalize → sbappendaddr_locked` and Asterinas `RangedAuxiliaryData`. `unix_send` is now a thin wrapper. See `net/src/unix_socket/mod.rs` and the regression tests in `core/src/syscall/tests.rs::test_unix_scm_rights_*`.
- **Implication for Phase 4.** The "preempt-on-enqueue exposes multi-step publish bugs" pattern applies to any future migration off ad-hoc wait queues. The Phase 4 typed-event bus must publish all dependent state inside the same critical section before the wake fires — same contract as `unix_sendmsg`.

---

## 1. Cross-phase conventions

These are the same rules the Phase 1+2 work followed; future agents should not deviate.

1. **No foreign-OS attribution in code or commit messages.** Linux/FreeBSD/Asterinas are fine references in this plan file (and in `plans/*`), but kernel comments must describe SlopOS's design directly. Per `feedback_no_os_attribution.md`.
2. **No plan references in code.** "Phase 3", "Phase 4.2" don't belong in source or scripts. The plan stays in `plans/`. Per `feedback_no_plan_references_in_code.md`.
3. **`#![forbid(unsafe_code)]` everywhere except `slopos-ostd/`.** Every new file outside `slopos-ostd` must compile under this attribute. The build gates `scripts/check_unsafe_outside_ostd.sh`, `scripts/check_alloc_dep.sh`, `scripts/check_stack_sizes.sh` (2 KiB threshold) enforce it.
4. **Allocation discipline.** Use `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, `PinBox` from `slopos-ostd::mm::heap`. Never `extern crate alloc;` outside OSTD. The in-place-init primitive is `slopos_ostd::Init<T, E>` — large structs must be constructed via `KBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack.
5. **Pre-commit `cargo fmt --all` is mandatory.** Stage reformatted files before commit. Per `CLAUDE.md`.
6. **Never commit unless the user explicitly asks.** Phase 1+2 work was a single unsigned mega-PR; Phase 3+ follow the same shape unless the user says otherwise.
7. **Test before declaring a phase done.** `just test` must finish with `0 failed`. `just check-framekernel` must finish with all gates green. New regression tests are listed under each phase's "verification" section — they are *not optional*.
8. **Atomic publish discipline.** Any new producer-side primitive (event bus, epoch-published table, XDP filter chain) that wakes a consumer must complete all dependent state writes *before* the wake call returns to the producer. The `unix_sendmsg` shape is the template.

---

## 2. Phase 3 — Epoch primitive + lock-free TCP demux

**Goal.** Make `tcp::table::find` (`net/src/tcp/table.rs`) lock-free on the read side. Today every RX packet that reaches `tcp::input` takes `TCP_SHARDS[h].lock()` plus `TCP_LISTENERS.lock()` — a per-packet SpinLock acquire that serialises all RX even when there's no logical contention. With Phase 1's threaded NAPI delivering bursts and Phase 2's lost-wakeup recheck driving the kthread harder, the per-packet shard lock is the next bottleneck.

**Acceptance signal.**
- `tcp::input` runs entirely inside one `let _g = NET_EPOCH.enter();` scope; no SpinLock acquire on the read path.
- `find` returns `Option<ConnId>` purely via `RcuCell::load` reads.
- All existing TCP tests (`net/src/tests/tcp_*`) stay green.
- New tests (see § 2.5) pass.

### 2.1 Epoch primitive — `slopos-ostd/src/sync/epoch.rs` (new)

FreeBSD `epoch(9)` shape, backed by SlopOS's existing RCU infrastructure. `slopos-ostd/src/sync/rcu.rs` already provides `synchronize_rcu`, `call_rcu`, `RcuCell<T>`, and quiescent-state reporting at LAPIC tick / context switch / idle / `RCU_QS_IPI_VECTOR=0xFB` — Phase 3 layers a *scoped* epoch on top so that a stalled net-stack reclaim cannot delay a VFS reclaim cycle and vice versa.

```rust
pub struct Epoch { /* per-CPU counters; reuses RCU QS infrastructure */ }
pub struct EpochGuard<'e> {
    _preempt: PreemptGuard,
    _lt: PhantomData<&'e Epoch>,
}

impl Epoch {
    pub const fn new() -> Self;
    pub fn enter(&self) -> EpochGuard<'_>;
    pub fn wait(&self);
    pub unsafe fn defer<F: FnOnce() + Send + 'static>(&self, f: F);
}

pub static NET_EPOCH: Epoch = Epoch::new();
```

**Key invariant:** `EpochGuard` holds a `PreemptGuard` from `slopos-ostd/src/cpu/preempt.rs`. Sleeping inside an epoch read-side is structurally forbidden — `PreemptGuard` panics on yield. This is the type-system version of "you cannot block inside `rcu_read_lock`"; the Phase 1 self-reschedule path already routes through `PreemptGuard` so the integration is automatic.

`defer` is `unsafe` because the caller asserts the deferred closure doesn't capture references that may be invalidated before the epoch completes. (`call_rcu` has the same shape.)

### 2.2 TCP table refactor — `net/src/tcp/table.rs`

Split `TcpShard` (today: 4 PCBs + 4 buffer pairs all inline) into two pieces:

- `TcpShardIndex` — small, just the 4-tuple → slot mapping. RCU-published via `RcuCell<TcpShardIndex>`. Cheap to clone; the write side clones, mutates, and `replace_take`s.
- `TcpShardPcbs` — the actual PCB state. Per-slot SpinLock — the write side and PCB-internal mutators take it; the read side never does.

```rust
pub static TCP_SHARDS:     [RcuCell<TcpShardIndex>; NUM_SHARDS] = ...;
pub static TCP_LISTENERS:  RcuCell<ListenerTable>                = ...;
pub static TCP_PCB_SLOTS:  [SpinLock<Option<Pcb>>; NUM_SHARDS * SLOTS_PER_SHARD] = ...;
```

**Read path** (`tcp::input` → `tcp::table::find`):

```rust
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    let _g = NET_EPOCH.enter();
    let shard = TCP_SHARDS[tcp_hash(tuple)].load()?;
    if let Some(slot) = shard.find_exact(tuple) {
        return Some(ConnId::shard(slot));
    }
    let listeners = TCP_LISTENERS.load()?;
    listeners.find_by_port(tuple.local_ip, tuple.local_port).map(ConnId::listener)
}
```

**Write side** (`install_established`, `release`, listener install/remove):
- Take the shard-level write `SpinLock` for serialisation only — readers never take it.
- Clone the immutable `TcpShardIndex`, mutate the clone, `RcuCell::replace_take` to publish.
- Hand the old `KBox<TcpShardIndex>` to `NET_EPOCH.defer(move || drop(old_box))` so it stays live until all in-flight readers have left the epoch.

**PCB-internal mutation** (TCB state machine, retransmit queues, write buffers) stays under the per-slot lock — the epoch protects only the demux *dispatch* read, not the full PCB.

### 2.3 Extend the predicate-purity gate

`scripts/check_wait_predicate_purity.sh` (added in Phase 1) needs one more ban: **acquiring a `SpinLock` over an `EpochGuard` is forbidden**. Sleeping inside an epoch via `SpinLock`-on-`PreemptGuard` is the new AB-BA. Add a grep for `Epoch::enter` followed (within ~30 lines, same scope) by `.lock()` on any `SpinLock` static, and fail the build.

### 2.4 Critical files

- `slopos-ostd/src/sync/epoch.rs` — new
- `slopos-ostd/src/sync/rcu.rs` — extend QS bookkeeping to back `Epoch::wait`; verify the IPI cadence is sufficient for epoch reclaim under load
- `slopos-ostd/src/cpu/preempt.rs` — confirm `PreemptGuard` panics on `yield` (it should; verify with a `#[should_panic]` host test)
- `net/src/tcp/table.rs:408-431` — split `TcpShard` → `TcpShardIndex` + `TcpShardPcbs`; switch read side to `RcuCell` + `NET_EPOCH.enter()`
- `net/src/tcp/mod.rs:112-129` — `tcp::input` read path uses `NET_EPOCH.enter()`
- `scripts/check_wait_predicate_purity.sh` — add the SpinLock-over-EpochGuard ban

### 2.5 Verification

- All existing TCP tests (`net/src/tests/tcp_*`, `tcp_keepalive_tests`, `tcp_live_tests`) green.
- New `stest!` `tcp::demux_lockfree_under_load` — N readers concurrently call `tcp::input`, one writer install/releases PCBs. Readers must never observe an inconsistent state (returned `ConnId` always corresponds to a live PCB or `None`).
- New `stest!` `epoch::reclaim_delayed_until_quiescence` — synthetic test that `Epoch::defer` runs only after `Epoch::wait` completes (proves the reclaim contract).
- `just check-framekernel` green including the extended purity gate.

### 2.6 Why this beats SlopOS-today

Today's `TcpShard::lock()` is taken per RX packet. At 1 Gbit/s = 80k pps minimum, that's 80k SpinLock acquires per second. The Phase 1 NAPI burst already delivers packets in batches of 64, so the per-shard contention is small today — but the moment a second NIC, a TCP retransmit thread, or a userland accept loop touches the same shard, the lock serialises everything. The RcuCell + EpochGuard read path scales linearly with reader count.

---

## 3. Phase 4 — Typed `KernelEvent` substrate

**Goal.** Replace 33+ static `[WaitQueue; CAP]` arrays (sockets, ttys, pipes, unix sockets, child-exit) with one typed kernel-wide event bus. The existing `WaitQueue` primitive stays as the implementation backend; the *API surface* becomes typed so producer/consumer mismatches are `rustc` errors instead of silent `void *key` drift à la Linux's `__wake_up_common`.

**Acceptance signal.**
- Zero `static ... [WaitQueue; CAP]` arrays outside `slopos-ostd/src/sync/`.
- Every wake call goes through `BUS.publish(KernelEvent::Variant { id })`.
- Every `FileOps::poll_*` returns a `Subscription` instead of indexing into a wait-queue array.
- `syscall_poll` / `syscall_select` use `Subscription::any([sub1, sub2, sub3])` as their primitive.
- All existing socket/pipe/tty/syscall tests stay green.

### 3.1 `KernelEvent` — `slopos-abi/src/event.rs` (new)

```rust
#[non_exhaustive]
pub enum KernelEvent {
    SocketRecv   { sock: SocketId },
    SocketSend   { sock: SocketId },
    SocketAccept { sock: SocketId },
    PipeRead     { pipe: PipeId },
    PipeWrite    { pipe: PipeId },
    TtyInput     { tty: TtyId },
    TtyOutput    { tty: TtyId },
    UnixSocket   { sock: UnixSocketId, dir: Dir },
    NetRxBatch   { dev: DevIdx },
    TimerFired   { wheel: WheelId, token: TimerToken },
    ChildExit    { task: TaskId },
}
```

`#[non_exhaustive]` so adding a variant later doesn't break dependent crates. Each id type (`SocketId`, `PipeId`, etc.) is a `repr(transparent)` newtype around the existing slot index.

### 3.2 `Pollee` trait — `slopos-ostd/src/sync/pollee.rs` (new)

```rust
pub trait Pollee {
    type Event: Into<KernelEvent>;
    fn subscribe(&self, ev: Self::Event) -> Subscription;
    fn publish(&self, ev: Self::Event);
}
```

`Subscription` is RAII:
- Construct: `enqueue_current()` on the backing `WaitQueue`.
- Drop: `remove_current()` on the same queue.
- `Subscription::wait()` blocks the current task until the event fires; `Subscription::any([sub1, sub2, ...])` is the poll/select primitive — block until *any* of the subscribed events fires, then return which one.

Internally `Pollee` stores typed `[WaitQueue; CAP]` arrays keyed by resource id. The surface is `bus.publish(KernelEvent::SocketRecv { sock })`, the implementation indexes `RECV_WQS[sock.0 % CAP].wake_all()`. The user-visible API is `KernelEvent`; the internal data structure is unchanged.

### 3.3 Migration sites

Every existing `static ... [WaitQueue; CAP]`:

| Current location | Today | Phase 4 |
|---|---|---|
| `net/src/socket.rs::RECV_WQS / ACCEPT_WQS / SEND_WQS` | indexed wait queues | `KernelEvent::SocketRecv / SocketAccept / SocketSend` |
| `drivers/src/tty/table.rs::TTY_INPUT_WAITERS / TTY_POLL_WAITERS / TTY_OUTPUT_WAITERS / TTY_WRITE_LOCKS` | per-TTY wait queues | `KernelEvent::TtyInput / TtyOutput` |
| `fs/src/pipe.rs::READER_WQS / WRITER_WQS` | per-pipe wait queues | `KernelEvent::PipeRead / PipeWrite` |
| `net/src/unix_socket/mod.rs::SOCKET_WQS` | per-unix-socket wait queues | `KernelEvent::UnixSocket { sock, dir }` |
| `slopos-ostd/src/sync/mutex.rs::Mutex<T>::waiters` | per-mutex contention | **stays** — this is the primitive layer, not a kernel-wide event |
| `slopos-ostd/src/task/kernel_task.rs::Task::waiters` | child-exit waiters | `KernelEvent::ChildExit { task }` |

### 3.4 Wake-side replacements

- `socket_wake_recv_hint(hint)` in `net/src/socket.rs:917` → `BUS.publish(KernelEvent::SocketRecv { sock })`
- `wake_input_and_poll(slot)` in `drivers/src/tty/io.rs` → `BUS.publish(KernelEvent::TtyInput { tty })`
- `notify_input_ready` in `drivers/src/tty/io.rs:190` → same
- ... and so on for every wake site

### 3.5 `FileOps::poll_*` migration

Every existing `poll_events` / `poll_register` / `poll_unregister` impl returns a `Subscription`. The syscall handlers (`core/src/syscall/fs/poll_ioctl_handlers.rs::syscall_poll` and `syscall_select`) take a list of `Subscription`s and call `Subscription::any` to block on the first to fire.

The atomic-publish discipline from `unix_sendmsg` applies: a producer must finish *all* state writes that affect the subscribed condition *before* calling `bus.publish` — otherwise the woken consumer can drain a partial state. Phase 4's contract: state-writes-then-publish, in one critical section, mirrors the rule that fixed SCM_RIGHTS.

### 3.6 Why this beats Linux's wake-up keying

Linux's `__wake_up_common(wq_head, mode, nr_exclusive, key)` keys on a `void *` parameter the producer and consumer must agree on out-of-band. epoll, AIO, futex all roll their own conventions. SlopOS's `KernelEvent` is a `rustc`-checked enum — mismatched producer/consumer is a compile error, not a "test it in production" exercise.

### 3.7 Critical files

- `slopos-abi/src/event.rs` — new (`KernelEvent` enum + id types)
- `slopos-ostd/src/sync/pollee.rs` — new (`Pollee` trait, `Subscription`, `Subscription::any`)
- `net/src/socket.rs:734-742` — migrate `RECV_WQS`/`ACCEPT_WQS`/`SEND_WQS`
- `drivers/src/tty/table.rs` — migrate `TTY_*_WAITERS`
- `fs/src/pipe.rs:70-71` — migrate `READER_WQS`/`WRITER_WQS`
- `net/src/unix_socket/mod.rs:55` — migrate `SOCKET_WQS` (the unix-socket atomicity contract from the recent fix must be preserved — `Subscription` publish happens in the same critical section as `unix_sendmsg`'s fd+data commit)
- `slopos-ostd/src/task/kernel_task.rs::Task::waiters` — migrate to `KernelEvent::ChildExit`
- All `FileOps::poll_*` impls — `net/src/socket_file_ops.rs:82-128`, `fs/src/pipe_file_ops.rs`, `drivers/src/tty_file_ops.rs:110+`, `net/src/unix_socket_file_ops.rs`
- `core/src/syscall/fs/poll_ioctl_handlers.rs` — `syscall_poll` / `syscall_select` use `Subscription::any`

### 3.8 Verification

- All existing socket/pipe/tty/unix-socket tests green (~50 tests across `net/src/tests/`, `fs/src/`, `drivers/src/tty/`, `core/src/syscall/tests.rs`).
- New `stest!` `event::typed_dispatch_socket_recv` — `BUS.publish(SocketRecv { sock })` wakes *only* the task subscribed to that exact sock id, not others.
- New `stest!` `event::multi_event_any_wait` — `Subscription::any([sub1, sub2])` wakes on either subscribed event.
- New `stest!` `event::scm_rights_atomicity_preserved` — repeats the `unix_sendmsg` SCM_RIGHTS atomic-delivery test through the new typed-event bus; data + fds + publish must all happen in one critical section.

---

## 4. Phase 5 — Rust-typed XDP

**Goal.** Safe-Rust packet filters, registered from userspace, executed inline in the NAPI kthread before L3 dispatch. No BPF VM, no bytecode verifier, no JIT — monomorphised Rust. The verifier is `rustc` + `forbid(unsafe_code)`.

**Acceptance signal.**
- An `#[xdp_filter]` Rust function can be linked into the kernel image.
- `run_napi_burst` calls `XDP.execute(&mut pkt_view)` between L2 and L3.
- A drop-all filter installed via `#[xdp_filter]` reliably suppresses `tcp::input` from seeing the packet.
- A pass-through filter does nothing observable.
- No BPF infrastructure, no JIT, no `unsafe` in any filter.

### 4.1 Hook point

In `run_napi_burst` (`drivers/src/virtio_net.rs`), just after `ingress::net_rx`'s EtherType demux (`net/src/ingress.rs:70`) and before `ipv4::handle_rx`:

```rust
match XDP.execute(&mut pkt_view) {
    XdpAction::Pass         => ipv4::handle_rx(dev, pkt, checksum_rx),
    XdpAction::Drop         => pool::recycle(pkt),
    XdpAction::Redirect(t)  => t.transmit(pkt),
    XdpAction::Tx           => virtnet_transmit(pkt),
}
```

### 4.2 Filter registration — `net/src/xdp/mod.rs` (new)

```rust
pub trait XdpFilter: Send + Sync + 'static {
    fn execute(&self, pkt: &mut PacketView<'_>) -> XdpAction;
}

pub static XDP: XdpHookChain = XdpHookChain::new();
```

`XdpHookChain` holds `RcuCell<KVec<KBox<dyn XdpFilter>>>` (uses the Phase 3 epoch infra). Lock-free read on the NAPI hot path. Filters are `forbid(unsafe_code)` Rust types loaded statically via a `linkme`-style distributed slice. Dynamic load via a module loader is future work.

### 4.3 `PacketView` — `net/src/xdp/packet_view.rs` (new)

Safe wrapper over the existing `PacketBuf` (`net/src/packetbuf.rs`):
- Immutable header parses (`PacketView::ethernet()`, `.ipv4()`, `.tcp()`).
- Mutable payload via `pull_header` / `push_header`.
- Checksum helpers from `net::checksum`.

Because filters are pure safe-Rust monomorphised at kernel link time, **the verifier is rustc**. No separate BPF verifier needed; the language's safety guarantees plus the `forbid(unsafe_code)` filter contract is the verifier.

### 4.4 `#[xdp_filter]` proc-macro — `net/src/xdp/macros.rs` (new)

```rust
#[xdp_filter]
fn drop_ssh(pkt: &mut PacketView<'_>) -> XdpAction {
    if let Some(tcp) = pkt.tcp() && tcp.dst_port() == 22 {
        XdpAction::Drop
    } else {
        XdpAction::Pass
    }
}
```

The proc-macro inserts the filter into a `linkme` distributed slice at compile time. Filters ship in the kernel image; the hook chain is built from the slice at boot.

### 4.5 Userspace registration

**Phase 5-A** (this plan): static `#[xdp_filter]` at compile time, no syscall, no dynamic load.

**Phase 5-B** (deferred, out of scope here): `sys_xdp_install(filter_id: u32)` syscall for dynamic load. Needs a module loader, which SlopOS doesn't have yet — defer.

### 4.6 Why this beats Linux XDP

| Concern         | Linux XDP                                  | SlopOS XDP                          |
|---|---|---|
| Memory safety   | BPF verifier (bug-prone, restrictive)      | rustc + `forbid(unsafe_code)`       |
| Performance     | BPF JIT (per-arch backend)                 | Direct Rust dispatch + LTO          |
| Expressiveness  | Limited (bounded loops, restricted types)  | Full Rust modulo `unsafe`           |
| Map types       | Custom BPF maps                            | Any Rust container (`KBox`, `KVec`) |
| Helpers         | Per-helper kernel-side `unsafe` wrappers   | None — filters use safe-Rust APIs   |

### 4.7 Critical files

- `net/src/xdp/mod.rs` — new (`XdpFilter`, `XdpAction`, `XdpHookChain`, `XDP`)
- `net/src/xdp/packet_view.rs` — new (`PacketView` over `PacketBuf`)
- `net/src/xdp/macros.rs` — new (`#[xdp_filter]` proc-macro placeholder; full impl may live in a separate proc-macro crate if needed)
- `drivers/src/virtio_net.rs` — `run_napi_burst` calls `XDP.execute` between L2 and L3
- `net/src/ingress.rs:70` — hook point

### 4.8 Verification

- New `stest!` `xdp::filter_drop_drops_packet` — install a drop-all filter; assert `tcp::input` is never called on a synthetic packet that would otherwise match an established connection.
- New `stest!` `xdp::filter_pass_falls_through` — install a pass-through filter; the original RX path runs end-to-end.
- New `stest!` `xdp::filter_chain_order` — multiple filters run in registration order; the first non-`Pass` action wins.
- All existing networking tests green (XDP defaults to no filters, so behaviour is unchanged).

---

## 5. End-state verification (after all three phases land)

```sh
# Phase 3 acceptance — no SpinLock acquires on TCP RX read path.
grep -rn '\.lock()' net/src/tcp/ | grep -v 'TCP_PCB_SLOTS'  # only PCB-write callers may remain
# Phase 4 acceptance — no ad-hoc wait-queue arrays outside the primitive layer.
grep -rn 'static .*WaitQueue' net/ fs/ drivers/ slopos-ostd/src/{sync,task}/  # empty outside slopos-ostd
# Phase 5 acceptance — XDP hook is wired and filter chain compiles.
grep -rn 'XDP\.execute' drivers/src/virtio_net.rs  # one hit, post-L2 pre-L3
just check-framekernel   # all gates green including the extended purity gate
just test                # all phases pass; new tests above included
```

---

## 6. Risk register

| Risk | Mitigation |
|---|---|
| `Epoch` reclaim falls behind under sustained TCP churn → memory exhaustion | Add a backpressure counter in `Epoch`; if `deferred_count > THRESHOLD`, `synchronize_rcu` synchronously on the write side. |
| Migrating 33 wait-queue arrays in one PR is too risky | Phase 4 can be sub-staged: pipe → tty → unix-socket → TCP socket → child-exit, one type per sub-stage with green tests between. |
| `linkme` distributed slices behave differently on the SlopOS link path | Verify early with a one-filter smoke test before designing the macro. If `linkme` is incompatible, fall back to `inventory`-style or a manual `pub const FILTERS: &[&dyn XdpFilter] = &[ ... ]` declared in `userland/src/xdp_filters.rs`. |
| The "preempt-on-enqueue exposes multi-step publish" bug pattern recurs in Phase 4 | Mandate the atomic-publish discipline (§ 1, item 8). Add a regression test for each migrated wake site mirroring `test_unix_scm_rights_atomic_delivery`. |
| `PreemptGuard` doesn't actually panic on yield | Catch in Phase 3 § 2.1 first verification step. Fix in `slopos-ostd/src/cpu/preempt.rs` before proceeding. |

---

## 7. References

- Linux RCU + `synchronize_rcu` semantics — `Documentation/RCU/whatisRCU.rst` (mainline).
- FreeBSD `epoch(9)` — `sys/sys/epoch.h`, `sys/kern/subr_epoch.c`.
- Linux `__wake_up_common` + `void *key` — `kernel/sched/wait.c`.
- Asterinas `Pollee` / `SocketEventObserver` — `kernel/src/process/signal/sig_pollee.rs` and surrounding modules.
- Linux XDP design — Høiland-Jørgensen et al., "The eXpress Data Path", CoNEXT '18.
- SlopOS Phase 1+2 implementation commits — the merge that landed `KernelIoToken`, `NapiWaker`, threaded NAPI, the unix-socket atomicity fix, and the regression tests under `core/src/syscall/tests.rs::test_unix_scm_rights_*`.
