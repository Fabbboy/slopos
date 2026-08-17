# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17 (original); last reviewed 2026-08-10.
Method: repository-wide static review (`grep`, `ast-grep`, targeted source inspection), plus NVD CVE lookups via `curl` + `jq`. The 2026-07-30 pass added a structured sweep: six reference briefs on proven kernel designs, fifteen per-subsystem auditors, then one adversarial verifier per candidate finding (two independent lenses for the most severe), each instructed to default to REFUTED and to locate the guard that would disprove the claim. Only findings that survived verification with a concrete attacker trigger are recorded here. The 2026-08-08 pass swept the authorization and resource-accounting surfaces specifically, over twelve prior-art clusters and seven adversarial lenses, and added 0044–0052; its remediation lived in the process-object and resource-accounting plans (both since landed and retired) and in `plans/authority-model.md`; the nine findings that had no framework dependency have since been fixed and removed under the policy below.

The 2026-08-09 pass swept the resource-accounting implementation as it landed (phases 1-5). Per-principal ceilings are now enforced on seven kinds — descriptor slots, object rows, tasks, processes, in-flight `SCM_RIGHTS` custody, pinned pages, and kernel metadata (task stacks) — which closes the class of "one unprivileged process exhausts a fixed global table and every other process is permanently denied" for those resources. The sweep found one defect in the new code, in `Charge::try_extend`: a saturating add capped the token's amount below what the account row had already been debited, so the difference would never be refunded. Not recorded as a finding — it required a single object holding 4 billion units, which no ceiling permits, so there was no attacker-reachable trigger; fixed by refusing the extension instead, which makes the reservation's own `Drop` return the debit.

The same pass swept the two structural changes that landed alongside it. AF_INET sockets now hold their own wait queues on a pinned, boot-allocated spine rather than sharing one of 64 folded buckets, which removes a cross-socket wake amplification (one event woke up to sixteen unrelated sockets) without changing the correctness argument — the fallback path is chosen once by `OnceLock::call_once` and cannot flip under a parked task, so no wake can be lost. Input event-queue slots are now claimed only at a syscall or focus change, never from the PS/2 IRQ handler: a claim there acquired one of 32 slots on behalf of a task that never asked, at a point with no principal and no errno path, which is the confused-deputy shape at queue-registration scope.

**Both residual exposures the previous pass recorded are now closed.** Keepalive DMA frames carry their own independent `PinnedBytes` charge (`KeepaliveFrames`), refunded where the frames are actually released rather than at ring teardown, and a retransmit's second in-flight DMA is charged as the second pin it is. `Pages` is charged per address space and enforced at 65 536 pages, so address-space growth is no longer bounded only by the frame allocator.

The 2026-08-10 pass swept the second implementation round — `Pages`, kernel-heap backing, keepalive charges, the reclaim tier, and `prlimit64`. Three defects were found and fixed; none is recorded as a finding, because none had an attacker-reachable trigger, and the policy below forbids scoring speculative issues:

1. **`shrink_clean` rebuilt the block-cache index with an allocating `KBTreeMap::insert`**, on the path that runs *because* allocation failed. A failed re-insert would have left a live cached block unreachable under its own number — a correctness fault, not a safety one, and unreachable in practice because the reclaimer is only driven after an allocation has already failed and before any caller depends on the index. Replaced with an in-place repair of the single entry `swap_remove` moves, which allocates nothing.
2. **`prlimit64` mapped an unconvertible `rlim_cur` to the no-limit sentinel.** Had it been reachable this would have been an unprivileged escape from every ceiling: the widest possible `setrlimit` would have switched enforcement off for the caller's own account. It is not reachable — the `EPERM` check against the published hard limit runs first and bounds the value below `u32::MAX` for every published resource — so it is fixed as defence in depth (clamp to the current limit, never raise) and pinned by a userland test rather than scored.
3. **The `Pages` audit was vacuous as first written**, comparing two numbers maintained by the same code. Rewritten to compare the account row against the summed maps, which is the pair a phantom debit separates.

The reclaim tier was reviewed specifically for the deadlock class it could introduce, since it runs on allocation-failure paths reachable from under other subsystems' locks. Both reclaimers are non-blocking by construction: the quarantine reclaimer takes the buddy lock it already owns, and the ext2 reclaimer `try_lock`s a sleeping mutex held across block I/O and returns zero rather than waiting. Reclaim is never driven from `try_charge` — the arena takes no locks by construction, and a hook there would give it an inbound edge from every charge site at once — but from the caller of a refused allocation, at a syscall boundary or the demand-fault path, each with a single bounded retry.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no backwards-compatibility or audit-trail obligations, so this ledger tracks **open findings only**. When a finding is resolved it is **removed** from this file (not retained as a `fixed` historical record). Internal IDs stay stable for findings that remain open, so gaps in the numbering are expected.

## Scoring Method

- CVSS version: 3.1 Base Score
- Formula used: Base score derived from Impact + Exploitability subscores with scope-aware rounding up to one decimal
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`
- Reusable scorer script: `scripts/cvss_calc.py`

SlopOS has no credential model, so "unprivileged local attacker" means any process that can execute code. Those are scored `AV:L/PR:L` by the usual convention. A panic in a `#![forbid(unsafe_code)]` crate is an availability impact, never a memory-safety one, and is scored accordingly — several findings that read as alarming are correctly `A:H` with `C:N/I:N`.

Findings with no attacker-reachable trigger are deliberately **absent** from this ledger even at high confidence, because the policy forbids presenting speculative issues as CVSS-scored vulnerabilities. Several such items — the safe raw-pointer vocabulary OSTD exports, macro-injected `unsafe` inside `forbid(unsafe_code)` crates, executable IST stacks, UEFI runtime pages mapped RWX — are real engineering defects tracked in `plans/` instead.

### Reusable scorer

```bash
python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
```

## Open findings at a glance

| ID | Score | Severity | Title |
|---|---:|---|---|
| [SLOPOS-2026-0012](#slopos-2026-0012) | 9.1 | CRITICAL | The in-kernel DNS resolver has effectively no anti-spoofing entropy and accepts responses from any host |
| [SLOPOS-2026-0017](#slopos-2026-0017) | 7.8 | HIGH | PCIDs are assigned from a wrapping 12-bit counter with no reuse tracking while every CR3 write is NOFLUSH |
| [SLOPOS-2026-0039](#slopos-2026-0039) | 7.6 | HIGH | Device-supplied PCI offsets are used to map MMIO without bounding them against the BAR |
| [SLOPOS-2026-0013](#slopos-2026-0013) | 7.4 | HIGH | No RFC 793 §3.9 sequence-acceptability check: a blind SYN tears down an established connection |
| [SLOPOS-2026-0020](#slopos-2026-0020) | 7.1 | HIGH | `execve` is not an address-space boundary |
| [SLOPOS-2026-0021](#slopos-2026-0021) | 7.1 | HIGH | Mount resolution is a textual prefix match on the unnormalised user path |
| [SLOPOS-2026-0037](#slopos-2026-0037) | 7.1 | HIGH | The compositor clipboard has no authorization |
| [SLOPOS-2026-0049](#slopos-2026-0049) | 5.5 | MEDIUM | `halt` and `reboot` have no authorization check, by three separate paths |
| [SLOPOS-2026-0043](#slopos-2026-0043) | 6.6 | MEDIUM | ext2 mounts and writes any image whose magic and geometry are sane, with no feature-compatibility gate |
| [SLOPOS-2026-0022](#slopos-2026-0022) | 6.3 | MEDIUM | ramfs recycles inode ids immediately on unlink while descriptors still name them |
| [SLOPOS-2026-0014](#slopos-2026-0014) | 5.9 | MEDIUM | TCP initial sequence numbers come from an invertible FNV chain |
| [SLOPOS-2026-0040](#slopos-2026-0040) | 5.9 | MEDIUM | virtio-net's RX ring shrinks monotonically and never refills |
| [SLOPOS-2026-0016](#slopos-2026-0016) | 5.5 | MEDIUM | AF_UNIX SCM_RIGHTS has no cycle policy, permanently leaking socket slots |
| [SLOPOS-2026-0029](#slopos-2026-0029) | 5.5 | MEDIUM | `klog` has no rate limiting and userland can drive it from a cli-held lock |
| [SLOPOS-2026-0030](#slopos-2026-0030) | 5.5 | MEDIUM | Ready-queue selection is strict priority with no aging backstop |
| [SLOPOS-2026-0031](#slopos-2026-0031) | 5.5 | MEDIUM | Futex buckets cap waiters at 16 and return ENOMEM, which every userland futex wrapper discards |
| [SLOPOS-2026-0015](#slopos-2026-0015) | 4.8 | MEDIUM | The RFC 5961 challenge-ACK budget is a single global counter |
| [SLOPOS-2026-0019](#slopos-2026-0019) | 4.7 | MEDIUM | `mprotect` issues no cross-CPU TLB shootdown |
| [SLOPOS-2026-0033](#slopos-2026-0033) | 4.7 | MEDIUM | `synchronize_rcu` allocates infallibly and is `call_rcu`'s own out-of-memory fallback |
| [SLOPOS-2026-0038](#slopos-2026-0038) | 4.7 | MEDIUM | Runtime display mode-set frees the old scanout while the vconsole still points at it |
| [SLOPOS-2026-0023](#slopos-2026-0023) | 4.4 | MEDIUM | ramfs `rename` has no ancestor check and leaks the displaced target |
| [SLOPOS-2026-0025](#slopos-2026-0025) | 4.4 | MEDIUM | ext2 `create` performs no duplicate-name check |
| [SLOPOS-2026-0042](#slopos-2026-0042) | 4.4 | MEDIUM | The reboot path never flushes the filesystem and the ext2 image carries no dirty-state word |
| [SLOPOS-2026-0024](#slopos-2026-0024) | 3.3 | LOW | ramfs silently truncates over-long names, creating unreachable, unreclaimable inodes |
| [SLOPOS-2026-0026](#slopos-2026-0026) | 3.3 | LOW | ext2 `unlink` never frees double- or triple-indirect blocks |
| [SLOPOS-2026-0027](#slopos-2026-0027) | 3.3 | LOW | O_APPEND is evaluated once at open and the file position has no lock |
| [SLOPOS-2026-0028](#slopos-2026-0028) | 3.3 | LOW | `stat`, `fstat` and `sys_info` copy uninitialized struct padding to userland |
| [SLOPOS-2026-0032](#slopos-2026-0032) | 3.3 | LOW | `FUTEX_WAIT` accepts a timeout and silently ignores it |
| [SLOPOS-2026-0036](#slopos-2026-0036) | 3.3 | LOW | A malformed compositor frame wedges that client's connection permanently |
## Open SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0006
- Title: ext2 inode/group descriptor size trust can panic on out-of-bounds slicing
- Status: open
- Confidence: 70 — evidence 35, exploitability 15 (depends on whether SlopOS mounts untrusted images), reproducibility 20. Below the 80 threshold for a guaranteed CVSS-scored issue.
- Evidence: untrusted on-disk `inode_size` / derived offsets used in slice indexing without validating `within + size <= block_size`. `fs/src/ext2/ondisk.rs` (`Inode::parse`, `GroupDesc::parse`, `effective_inode_size` clamps `inode_size == 0` up to 128 but does not bound it from above against `block_size`) and the inode-table offset math in `fs/src/ext2/inode.rs` / `fs/src/ext2/mod.rs`.
- Impact: malformed-image-triggered out-of-bounds slice index. Because `fs/` is `#![forbid(unsafe_code)]`, this is a bounded-slice **panic** (DoS), never memory unsafety/UB.
- CVSS vector/score: not assigned because confidence is below 80.
- Remediation (proposed): validate `effective_inode_size() as u32 <= block_size` and bound each `within + size` against the containing block before slicing. Note that SLOPOS-2026-0043 covers the adjacent absence of a feature-compatibility gate on the same mount path.

### SLOPOS-2026-0012
- Title: The in-kernel DNS resolver has effectively no anti-spoofing entropy and accepts responses from any host
- Status: open
- Confidence: 90 — evidence 40 (the id source, the port derivation and the response validation all read directly), exploitability 28 (off-path spoofing needs no race because the id is predictable from boot), reproducibility 22 (requires network position to observe or predict the query)
- CVSS vector/score: `CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:N` — **9.1 CRITICAL**
- Impact: The transaction id starts at a fixed boot constant and increments; the source port is a pure function of that id, so it contributes no independent entropy. Response validation checks only `qr`, the id, and `rcode` — no source-IP, destination-IP or destination-port check. One spoofed UDP datagram with source port 53 poisons the kernel resolver cache, redirecting every subsequent name resolution for that name.
- Evidence:
  - net/src/dns.rs:39 — `static QUERY_ID: AtomicU16 = AtomicU16::new(0x4242);` — a fixed boot constant
  - net/src/dns.rs:662,686 — `QUERY_ID.fetch_add(1, Ordering::Relaxed)` is the only ID source; the first query after boot always has ID 0x4242
  - net/src/dns.rs:796 — `let src_port = 49152 + (resolver.query_id() % 16384);` — the source port is a pure function of the transaction ID, contributing no independent entropy
  - net/src/dns.rs:336-345 — `dns_parse_response` validates only `qr`, `id == expected_id`, and `rcode`
  - drivers/src/virtio_net.rs:930-938 — the interception: `if src_port == net::dns::DNS_PORT { … DNS_RX_BUF … DNS_RX_EVENT.signal(); }` — no check of source IP against the configured server, no check of destination port against the query's source port
- Repro:
  Observe or predict the query id (the first query after boot is always 0x4242 and it increments), then send a UDP datagram from any address with source port 53 carrying that id and the attacker's answer. It is accepted and cached.
- Remediation: Seed the transaction id from the CSPRNG rather than a constant, randomise the source port independently (RFC 5452), and validate that the response's source address and port match the query's destination. Longer term this belongs in userland, not the kernel.

### SLOPOS-2026-0013
- Title: No RFC 793 §3.9 sequence-acceptability check: a blind SYN tears down an established connection
- Status: open
- Confidence: 90 — evidence 40 (the segment handler read in full; the absence of the acceptability test confirmed), exploitability 26 (needs the four-tuple, which means guessing a ~16-bit ephemeral port), reproducibility 24 (one packet once the tuple is known)
- CVSS vector/score: `CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:H/A:H` — **7.4 HIGH**
- Impact: `DataState::on_segment` has no acceptability test, so any spoofed SYN on the four-tuple tears down an established connection regardless of sequence number, `ts_recent` can be poisoned out of window, and blind data/FIN is accepted with any ACK number. This is precisely what RFC 5961 §4 exists to prevent.
- Evidence:
  - net/src/tcp/pcb/data.rs:318-320 — `if hdr.is_syn() { return Self::on_unexpected_syn(pcb, hdr, actions); }` runs before any sequence test; `_hdr` is unused inside the handler
  - net/src/tcp/pcb/data.rs:422-431 — `on_unexpected_syn` unconditionally emits a RST, sets `actions.release = true`, and raises `RESET_RECEIVED`
  - net/src/tcp/pcb/mod.rs:100-121 — `Pcb::on_segment` dispatches straight to the per-state handler; there is no in-window filter anywhere on the path
  - net/src/tcp/mod.rs:100-139 — `tcp::input` does a 4-tuple lookup and dispatches; no window check
  - net/src/tcp/pcb/data.rs:335-347 — `ts_recent` is updated from the incoming TSval gated only on `seq_le(hdr.seq_num, data.last_ack_sent) || data.last_ack_sent == 0`, with no window test
- Repro:
  Given an established connection and a guessed ephemeral port, one spoofed IPv4/TCP frame with the SYN flag set on that four-tuple resets it. Combined with the ISN weakness below, the port guess becomes cheaper still.
- Remediation: Implement the RFC 793 §3.9 acceptability test as the first gate in `on_segment`, then layer RFC 5961: challenge-ACK an in-window SYN rather than resetting, require an exact-match RST sequence, and reject data outside the receive window.

### SLOPOS-2026-0014
- Title: TCP initial sequence numbers come from an invertible FNV chain
- Status: open
- Confidence: 88 — evidence 38 (the generator read and the truncation-closure argument checked), exploitability 26 (needs two observed connections plus the on-wire timestamp), reproducibility 24 (algebraic, deterministic once the observations are in hand)
- CVSS vector/score: `CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:H/A:N` — **5.9 MEDIUM**
- Impact: The ISN is an FNV-1a chain whose low-32-bit computation is closed under truncation, so the output depends only on the low 32 bits of the boot secret. Two observed connections recover it, making every future ISN predictable — which the module doc claims is off-path-unpredictable. Predictable ISNs make blind data injection and connection hijacking practical, especially in combination with the missing acceptability check above.
- Evidence:
  - net/src/tcp/isn.rs:61-68 — `FNV_PRIME = 0x0000_0100_0000_01B3`; `fnv_mix(h,b) = (h ^ b).wrapping_mul(FNV_PRIME)`
  - net/src/tcp/isn.rs:72-98 — `generate_isn` seeds `h = FNV_OFFSET ^ boot_secret()`, mixes 12 known 4-tuple bytes, and returns `(h as u32).wrapping_add(drift)` where `drift = (monotonic_ns() / 4_000) as u32`
  - net/src/tcp/isn.rs:21-25 — the module doc: "This is intentionally **not** a keyed hash … The design is strictly better than the predictable counter it replaces", and :4-5 claims the result is "unpredictable to off-path attackers"
  - net/src/tcp/mod.rs:366 — the outgoing SYN carries `.with_timestamp(clock::now_ms() as u32, 0)`
  - net/src/tcp/pcb/listen.rs:121-123 — the SYN-ACK carries `syn_ack.timestamp = Some((now_ms as u32, tsval))` whenever the peer offered timestamps
- Repro:
  Send a SYN to any open port with the RFC 7323 timestamp option set (or induce an outbound `connect()`, which always sends a timestamp), record the returned ISN and TSval, repeat once, then solve for the low 32 bits of the secret.
- Remediation: Replace the FNV chain with a keyed cryptographic PRF over the four-tuple, as RFC 6528 specifies — SipHash-2-4 with a boot-time random key is the standard choice and is what Linux uses (`secure_tcp_seq`).

### SLOPOS-2026-0015
- Title: The RFC 5961 challenge-ACK budget is a single global counter
- Status: open
- Confidence: 85 — evidence 38 (the counter and its scope read directly), exploitability 24 (classic off-path side channel requiring a co-resident connection and probing), reproducibility 23 (statistical, needs many probes)
- CVSS vector/score: `CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:L/I:L/A:N` — **4.8 MEDIUM**
- Impact: The challenge-ACK budget is one un-jittered process-global counter. An off-path attacker holding its own connection to the host can observe budget consumption and infer a victim connection's `rcv_nxt` — CVE-2016-5696 verbatim. The in-tree doc cites Linux's pre-fix behaviour as the model.
- Evidence:
  - net/src/tcp/challenge_ack.rs:86-89 — `static EPOCH_START: AtomicU64` and `static CHALLENGE_COUNT: AtomicU32`, both process-global
  - net/src/tcp/challenge_ack.rs:96-110 — `try_challenge_ack(now_ms)` increments the single global counter and returns `prev < CHALLENGE_ACK_LIMIT`
  - net/src/tcp/challenge_ack.rs:26-29 — the module doc: "enforces a global per-epoch cap on challenge ACKs (default 1 000 per second, matching Linux's `tcp_challenge_ack_limit`)"
  - net/src/tcp/pcb/data.rs:924-933 — the only consumer: an in-window-but-inexact RST consumes a token from the global budget
  - net/src/tcp/challenge_ack.rs:100 — `if now_ms >= epoch.wrapping_add(EPOCH_MS) || epoch == 0` — while `uptime_ms()` is still 0 (early boot) the `epoch == 0` arm makes the limiter unconditionally permissive
- Repro:
  Open a normal connection to the host, then probe with spoofed in-window RSTs for the victim tuple while measuring challenge-ACKs received on the attacker's own connection; the shared counter leaks whether each probe was in window.
- Remediation: Make the budget per-connection and add jitter, as Linux did in commit 75ff39c ('tcp: make challenge ack less predictable'). Update the doc comment, which currently points at the vulnerable design.

### SLOPOS-2026-0016
- Title: AF_UNIX SCM_RIGHTS has no cycle policy, permanently leaking socket slots
- Status: open
- Confidence: 83 — evidence 38 (the fd-passing path and both fixed tables read directly), exploitability 26 (single unprivileged process, ~15 iterations), reproducibility 24 (deterministic, no race)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5 MEDIUM**
- Impact: A socket fd passed through SCM_RIGHTS into its own socketpair creates a reference cycle that nothing collects. Each cycle permanently leaks 2 of 32 socket slots and 1 of 16 pair entries; roughly 15 iterations exhaust both tables, denying AF_UNIX — and therefore the compositor connection — to every process until reboot.
- Evidence:
  - net/src/unix_socket_file_ops.rs:26-30 — `impl Drop for UnixSocketBacking { fn drop(&mut self) { let _ = unix_socket::unix_close(self.handle); } }` — the endpoint is closed only when the last `FileRef` drops
  - net/src/unix_socket/mod.rs:929 — `unix_close` is what calls `slots.remove(handle.handle())`; the slot is not freed by the `close(2)` syscall itself
  - net/src/unix_socket/pair.rs:46-65 — `AncillaryQueue` holds owning `FileRef`s until the receiver drains them
  - abi/src/event.rs:19 — `pub const MAX_UNIX_SOCKETS: usize = 32;`
  - net/src/unix_socket/mod.rs:543-560 — `unix_sendmsg` accepts any `KVec<FileRef>`; nothing inspects the file kind
- Repro:
  Single process, no cooperation: create a socketpair, `sendmsg` each end's fd through the other with SCM_RIGHTS, close both fds. Repeat ~15 times.
- Remediation: Either refuse to pass an AF_UNIX socket fd through SCM_RIGHTS (the cheap policy), or implement the AF_UNIX garbage collector Linux carries in `net/unix/garbage.c` for exactly this cycle. Also reclaim in-flight fds on process exit.

### SLOPOS-2026-0017
- Title: PCIDs are assigned from a wrapping 12-bit counter with no reuse tracking while every CR3 write is NOFLUSH
- Status: open
- Confidence: 87 — evidence 38 (the counter, the mask, the NOFLUSH bit and the dead ASID pool all read directly), exploitability 26 (4096 address-space creations, unprivileged, but only on CPUs where PCIDE is enabled), reproducibility 23 (deterministic once the counter wraps; the observable effect depends on which stale translation is reused)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:C/C:H/I:H/A:H` — **7.8 HIGH**
- Impact: PCIDs come from a global monotonic counter masked to 12 bits, with no reuse or flush tracking, and every CR3 write sets the NOFLUSH bit. Address-space creation number 4096 receives a PCID still holding another address space's cached translations, which are then used without a flush — a cross-address-space stale-TLB condition. The correct Linux-style ASID pool sits beside it as dead code, and the comment claims PCIDs are never reused.
- Evidence:
  - slopos-ostd/src/mm/vm_space.rs:287-298 — comment "PCID assignment. Monotonic counter, **never reused**, masked to the architectural 12 bits." then `fn alloc_pcid() -> Pcid { let raw = NEXT_PCID.fetch_add(1, Relaxed); Pcid::new((raw & 0x0FFF) as u16) }`. The mask *is* the reuse: allocation 4097 collides with allocation 1
  - slopos-ostd/src/arch/x86_64/cr3.rs:20-28 — `Pcid::KERNEL = Pcid(0)`; `NEXT_PCID` starts at 1, so `4096 & 0x0FFF == 0` hands a user address space the kernel's PCID
  - slopos-ostd/src/mm/vm_space.rs:336 — `pcid: alloc_pcid()` in `VmSpace::new`, i.e. one PCID consumed per process creation, never released
  - slopos-ostd/src/mm/vm_space.rs:539-551 — `activate` computes `pcide_enabled` from live CR4 and passes it as `no_flush` to `write_cr3_pcid`, so on PCID-capable hardware *every* address-space switch is NOFLUSH
  - slopos-ostd/src/arch/x86_64/cr3.rs:58-63 — `if no_flush { value |= 1u64 << 63; }`
- Repro:
  On a CPU where CR4.PCIDE is enabled at boot, spawn or fork 4096 times; each `create_process_vm` consumes one PCID. Note this interacts with entry 0010 — the pid ceiling is hit first, so the pid fix must land alongside this one.
- Remediation: Use the existing ASID pool: allocate PCIDs from it, track which CPU last loaded each, and flush on reuse (or drop NOFLUSH when handing out a recycled PCID). Linux's `tlb_state`/`ctx_id` generation scheme is the reference.


### SLOPOS-2026-0019
- Title: `mprotect` issues no cross-CPU TLB shootdown
- Status: open
- Confidence: 90 — evidence 38 (the cursor protect path confirmed to fire no hook, and the sole consumer confirmed to issue none), exploitability 24 (multi-threaded process on SMP), reproducibility 26 (deterministic given two threads on two CPUs)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:N/I:H/A:N` — **4.7 MEDIUM**
- Impact: `CursorMut::protect` fires no TLB hook and the only consumer issues no shootdown, so after a thread narrows a mapping's permissions the other CPUs keep the old, wider translation. A page made read-only remains writable from another CPU for as long as its entry survives. Confined to the caller's own address space, so this is an intra-process integrity failure rather than a cross-process one.
- Evidence:
  - slopos-ostd/src/mm/vm_space.rs:956-998 `CursorMut::protect` — after `pte.set_flags_only(flags)` it calls only `flush_leaf_local::<S>(self.cur)` (:995), which is `tlb::flush_local` on the *current* CPU (vm_space.rs:1006-1012)
  - slopos-ostd/src/mm/vm_space.rs:920-931 — `CursorUnmapHook::after_unmap` is fired only from `unmap`, only `if was_user`. `protect` has no hook of any kind, so a consumer gets no notification that a permission downgrade needs a shootdown
  - mm/src/user_mappings.rs:268-300 `ostd_protect_range_4kb` — loops `cursor.protect::<Size4Kb>(prop)` and returns `Ok(())`; no TLB call
  - mm/src/process_vm.rs:2634-2650 `process_vm_mprotect` — calls `ostd_protect_range_4kb` then `return 0`; no `tlb::flush_all_for_process`, no `tlb::flush_all`
  - core/src/syscall/memory_handlers.rs:79-90 `syscall_mprotect` — no flush of any kind. Note that the frame-reuse quarantine does not help here: the frame is still mapped and still owned by this process, so nothing frees it and nothing gates its reuse. Permission downgrades need an actual shootdown.
- Repro:
  Two threads sharing a VmSpace on different CPUs. T1 writes page P (caching a writable entry on CPU1). T0 calls `mprotect(P, PROT_READ)`. T1's next write to P still succeeds.
- Remediation: Fire the same shootdown hook the unmap path uses. `CursorMut::protect` is the right place, so every consumer inherits it.

### SLOPOS-2026-0020
- Title: `execve` is not an address-space boundary
- Status: open
- Confidence: 88 — evidence 38 (the complete set of state-reset operations in `do_exec` enumerated, and the loader's single unmap read), exploitability 26 (ordinary exec of an attacker-chosen binary), reproducibility 24 (deterministic; not covered by tests because the shell uses spawn_path)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N` — **7.1 HIGH**
- Impact: `do_exec` unmaps exactly the code window and nothing else. `teardown_inner_mappings` — which drains the VMA map and resets the heap — is never invoked on the exec path. The new program image inherits the previous image's entire heap, mmap arena, shared-memfd mappings and SlopRing mappings, so exec preserves rather than severs whatever the old image had mapped.
- Evidence:
  - core/src/exec/mod.rs:474-486 — the complete set of state-reset operations in `do_exec` is `process_vm_load_elf_data`, `process_vm_reset_stack`, `fileio_close_on_exec`
  - mm/src/process_vm.rs:1321-1330 — `unmap_existing_code_region` unmaps exactly `[PROCESS_CODE_START_VA, PROCESS_DATA_START_VA)`
  - mm/src/process_vm.rs:610-619 — `teardown_inner_mappings` drains the VMA map and resets heap state, and is called only from `destroy_process_vm`
  - core/src/exec/mod.rs:474-475 — `process_vm_load_elf_data(process_id, elf_data.as_slice(), entry_out)` replaces the caller's mappings; every failure after this line returns `Err` to a process whose old image is gone
  - core/src/exec/mod.rs:477-479 — `if process_vm_reset_stack(process_id) != 0 { return Err(ExecError::NoMem) }` — after the load
  - core/src/exec/mod.rs:481 — `setup_user_stack(...)?` — can return `NoMem` (`:523`, `:532`), `TooManyArgs` (`:514-516`) or `Fault` (`:507`, `write_to_user_stack` at `:599-601`), all after the load
- Repro:
  In a process holding a MAP_SHARED memfd mapping with a known byte pattern, `exec` a second binary and read the same address; the pattern is still there.
- Remediation: Add `process_vm_reset_for_exec(pid)` reusing `teardown_inner_mappings`: drain the whole VMA map (running the shared-mapcount decrement so memfd counts stay correct), unmap every user range rather than just the code window, reset heap_start/heap_end/heap_break, and re-randomise the layout. Linux's `exec_mmap()` installs a fresh `mm_struct` for exactly this reason.

### SLOPOS-2026-0021
- Title: Mount resolution is a textual prefix match on the unnormalised user path
- Status: open
- Confidence: 93 — evidence 40 (the match, the single-resolution walk and the `..` handling all read directly), exploitability 28 (one syscall with a crafted path), reproducibility 25 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N` — **7.1 HIGH**
- Impact: `resolve_mount` does a longest-prefix byte comparison against the raw user path with no canonicalisation, and `resolve_path` selects a filesystem exactly once, so `..` can never cross a mount. `//tmp/x`, `/./tmp/x` and `/dir/../tmp/x` all miss the `/tmp` mount and land on the root filesystem's shadowed directory instead.
- Evidence:
  - fs/src/vfs/mount.rs:102-111 — `let matches = if mp_path == b"/" { true } else if path.len() >= mp_path.len() { &path[..mp_path.len()] == mp_path && (path.len() == mp_path.len() || path.get(mp_path.len()) == Some(&b'/')) }` — a raw byte-prefix compare against the unnormalised user string
  - fs/src/vfs/mount.rs:212-225 — `resolve_mount` splits the path once, textually, and hands the remainder to the matched filesystem
  - fs/src/vfs/path.rs:14-32 — `resolve_path` calls `resolve_mount(path)` exactly once, then walks components entirely inside that one filesystem; a mount point crossed mid-walk is never detected
  - fs/src/vfs/path.rs:23-29 — `..` is resolved by `fs.lookup(current_inode, b"..")` *within the already-selected filesystem*, so it can never leave or enter a mount
  - fs/src/vfs/init.rs:21-28 — `/` (ramfs or ext2), `/tmp` (ramfs), `/dev` (devfs) are the three mounts this applies to
- Repro:
  `mkdir("/tmp")` creates a real directory on the root filesystem underneath the mount point. Writes through `//tmp/f` then go to that shadowed directory while reads through `/tmp/f` go to the ramfs mount — two different files behind one visible path.
- Remediation: Canonicalise the path before mount resolution (collapse `//`, `.`, and resolve `..` lexically against the mount table), and resolve mounts per component during the walk rather than once up front, so a mount crossed mid-path is honoured.

### SLOPOS-2026-0022
- Title: ramfs recycles inode ids immediately on unlink while descriptors still name them
- Status: open
- Confidence: 94 — evidence 40 (the free and the allocator read directly, and the id reuse confirmed), exploitability 26 (needs the unlink/create sequence, trivially arranged), reproducibility 28 (deterministic given an empty directory)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:H/I:H/A:N` — **6.3 MEDIUM**
- Impact: `unlink` frees the inode slot immediately and `alloc_inode` hands the same id to the next `create`. A descriptor left open across unlink+create silently reads and writes a different file. There is no per-file generation, so nothing detects the substitution.
- Evidence:
  - fs/src/ramfs/mod.rs:399 — `unlink` ends with `inner.inodes[target_id as usize].reset();`, and `reset` (:56-66) sets `in_use = false` and frees the data
  - fs/src/ramfs/mod.rs:153-159 — `alloc_inode` scans `(ROOT_INODE+1)..len` and returns the *first* `!in_use` slot, i.e. the id just freed
  - fs/src/vfs_file_ops.rs:21-24 — `struct OpenVnode { fs: &'static dyn FileSystem, inode: InodeId }` — an open fd stores a bare inode number with no generation
  - fs/src/vfs/traits.rs:8 — `pub type InodeId = u64;` — no generation/epoch field anywhere
  - fs/src/vfs/ops.rs:6-9 — `VfsHandle { pub inode: InodeId, pub fs: ... }` likewise
- Repro:
  With `/tmp` empty: `fd = open("/tmp/a", O_RDWR|O_CREAT)`; `unlink("/tmp/a")`; `open("/tmp/b", O_RDWR|O_CREAT)` — which reuses the freed slot; now `write(fd, ...)` writes into `/tmp/b`.
- Remediation: Keep the inode alive while descriptors reference it (a refcount released by the last close, which is what POSIX requires of unlink), or add a generation counter to the inode id so a stale descriptor fails rather than aliasing. The Handle/HandleTable generation machinery already in `slopos-ostd` is the right vehicle.

### SLOPOS-2026-0023
- Title: ramfs `rename` has no ancestor check and leaks the displaced target
- Status: open
- Confidence: 90 — evidence 38 (the rename path read in full, all three missing checks confirmed), exploitability 26 (two syscalls), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:L` — **4.4 MEDIUM**
- Impact: `rename` performs no ancestor, self- or target-type check and never frees the displaced target inode. A directory can be spliced into its own descendant, producing an unreachable and unremovable cycle, and every overwrite leaks an inode out of the fixed table.
- Evidence:
  - fs/src/ramfs/mod.rs:454-518 — the whole `rename` body; there is no check that `target_inode` is not an ancestor of `new_parent`
  - fs/src/ramfs/mod.rs:479-483 — `if new_parent_node.lookup(new_name).is_ok() { inner.get_inode_mut(new_parent)?.remove_dir_entry(new_name)?; }` — the displaced entry is unlinked from the directory but its `RamInode` is never `reset()`
  - fs/src/vfs/ops.rs:130-141 — `vfs_rename` does no ancestor check either; it only compares the two filesystems
  - fs/src/ramfs/mod.rs:161-163 — `RAMFS_MAX_INODES = 4096` is the hard ceiling the leak counts against
  - fs/src/ramfs/mod.rs:493-514 — the directory case fixes up `..` and nlink but performs no type checks: a file may be renamed over a directory and a directory over a non-empty directory
- Repro:
  `mkdir("/tmp/a"); mkdir("/tmp/a/b"); rename("/tmp/a", "/tmp/a/b/c")` creates a cycle unreachable from the root. Repeated overwrite renames exhaust the inode table.
- Remediation: Reject a rename whose destination is a descendant of the source (Linux walks the ancestor chain under the rename lock and returns EINVAL), require matching types, and release the displaced target inode.

### SLOPOS-2026-0024
- Title: ramfs silently truncates over-long names, creating unreachable, unreclaimable inodes
- Status: open
- Confidence: 92 — evidence 40 (the truncation and the `name_len == name.len()` lookup comparison both read directly), exploitability 26 (one syscall in a loop), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L` — **3.3 LOW**
- Impact: `create` truncates the name to 32 bytes and returns success, but lookup, unlink and duplicate detection all compare `entry.name_len == name.len()`, so the entry can never be matched by the name that created it. The file occupies a directory slot and an inode permanently, and repeated creation exhausts the table. ext2 rejects the same name with ENAMETOOLONG and `vfs/mount.rs` rejects it too — ramfs is the outlier.
- Evidence:
  - fs/src/ramfs/mod.rs:84-86 — `let len = name.len().min(MAX_NAME_LEN); entry.name[..len].copy_from_slice(&name[..len]); entry.name_len = len;` — truncate and return Ok
  - fs/src/lib.rs:5 — `pub const MAX_NAME_LEN: usize = 32;`
  - fs/src/ramfs/mod.rs:78 — the duplicate check is `entry.name_len == name.len() && entry.name[..entry.name_len] == *name`, which a 40-byte query can never satisfy against a 32-byte stored name
  - fs/src/ramfs/mod.rs:109 (`lookup`) and :98 (`remove_dir_entry`) use the identical comparison
  - fs/src/ramfs/mod.rs:161-163 — `RAMFS_MAX_INODES = 4096` with `alloc_inode` returning `NoSpace` past it
- Repro:
  `open("/tmp/" + "A"*40, O_CREAT|O_WRONLY)` succeeds; the subsequent `open("/tmp/" + "A"*40, O_RDONLY)` fails, and `unlink` cannot remove it. Loop to exhaust the inode table.
- Remediation: Return `VfsError::NameTooLong` when the name exceeds `MAX_NAME_LEN`, matching ext2 and the mount-table check. Raising `MAX_NAME_LEN` to 255 instead is defensible but the length gate is required either way.

### SLOPOS-2026-0025
- Title: ext2 `create` performs no duplicate-name check
- Status: open
- Confidence: 89 — evidence 38 (the create path read; the absent check confirmed), exploitability 24 (one syscall on an ext2 mount), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:L` — **4.4 MEDIUM**
- Impact: `create_inode_entry` writes a directory record without scanning for an existing name, so a second `mkdir` of an existing name succeeds and produces two records with the same name and two inodes. Lookup returns whichever comes first; the second inode becomes unreachable, and the on-disk image is left inconsistent for any other ext2 implementation.
- Evidence:
  - fs/src/ext2/mod.rs:336-430 — `create_inode_entry` validates the name length, reads the parent, allocates an inode, and calls `dir::append_dir_entry`. There is no `dir::lookup_child` pre-check anywhere in the function.
  - fs/src/ext2/dir.rs:198-297 — `append_dir_entry` searches for free slack and writes the record; it never compares names
  - fs/src/vfs/ops.rs:114-118 — `vfs_mkdir` calls `parent.fs.create(...)` directly with no existence check of its own
  - fs/src/ext2_vfs.rs:206-215 — the ext2 `FileSystem::create` impl is a thin passthrough to `create_directory`/`create_file`
  - fs/src/ramfs/mod.rs:336-338 — ramfs, by contrast, *does* check (`if parent_inode.lookup(name).is_ok() { return Err(AlreadyExists) }`)
- Repro:
  On an ext2-backed mount, `mkdir("/x")` twice. Both succeed; the directory then contains two `x` records.
- Remediation: Scan the directory for the name before creating, returning EEXIST — the lookup helper needed to do this already exists in `fs/src/ext2/dir.rs`.

### SLOPOS-2026-0026
- Title: ext2 `unlink` never frees double- or triple-indirect blocks
- Status: open
- Confidence: 82 — evidence 38 (the truncate/free path read; only direct and single-indirect handled), exploitability 22 (requires files past the single-indirect reach, ~4 MiB at 4 KiB blocks), reproducibility 22 (deterministic but needs enough free space to demonstrate)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L` — **3.3 LOW**
- Impact: Every block reachable through `i_block[13]` or `i_block[14]` is permanently leaked on delete. A file larger than the single-indirect reach can be created and deleted repeatedly to exhaust the filesystem, with no way to recover the space short of reformatting.
- Evidence:
  - fs/src/ext2/mod.rs:517-573 — `release_file_blocks` frees `inode.block[0..12]` (`.take(12)`, :518) and then handles `inode.block[12]` (:529-571). `inode.block[13]` and `inode.block[14]` are never touched.
  - fs/src/ext2/blockmap.rs:41-56 — `block_to_path` fully supports depth 3 (`DINDIRECT_IDX`) and depth 4 (`TINDIRECT_IDX`)
  - fs/src/ext2/blockmap.rs:127-166 — `ensure_data_block` allocates the intermediate indirect blocks for any depth, so `write_file` really does populate `block[13]`/`block[14]`
  - fs/src/ext2/mod.rs:463 — `unlink_entry` calls `release_file_blocks` as its only block-reclaim step
  - fs/src/ext2/file.rs:143-156 — `file::truncate` *does* handle depths 2 and 3 via `free_indirect`, but that function has zero callers (see the unwired-code finding)
- Repro:
  Create a file larger than the single-indirect reach, write it fully, delete it, and observe the free-block count does not return to its prior value. Repeat to exhaust the image.
- Remediation: Extend the truncate path to walk and free the double- and triple-indirect trees. The block-map walker in `fs/src/ext2/blockmap.rs` already knows how to traverse them for reads.

### SLOPOS-2026-0027
- Title: O_APPEND is evaluated once at open and the file position has no lock
- Status: open
- Confidence: 85 — evidence 38 (the one-shot seek and the unlocked offset both read directly), exploitability 24 (two descriptions on the same file), reproducibility 26 (deterministic, no race needed)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:N` — **3.3 LOW**
- Impact: O_APPEND is a one-shot seek-to-end at open rather than an atomic append at each write, and the shared offset is unlocked. Two descriptions on one file — or one description shared by two runnable tasks — overwrite each other's data. Append-only log files are not append-only.
- Evidence:
  - fs/src/fileio/fdops.rs:42-48 — `if flags.contains(OpenMode::APPEND) { match ops.size(handle) { Some(size) => position = size, ... } }` — the append offset is captured *once*, at open
  - fs/src/fileio/fdops.rs:295 — `let used_offset = if seekable { open_file.position() };` — the write path reads the stored position, never re-queries the size, and ignores the APPEND bit
  - fs/src/vfs_file_ops.rs:142 — the ext2/ramfs `FileOps::write` signature takes `_flags: u32` and discards it, so the append bit never reaches a filesystem either
  - fs/src/fileio/fdops.rs:295-307 — read/modify/write of the shared offset is `position()` → `ops.write(...)` → `position.fetch_add(rc)`, with no lock held across the three steps
  - fs/src/fileio/fdops.rs:396-408 — `file_seek_fd` does `snap.position()` then `.store(new_pos)`, a non-atomic RMW for `SEEK_CUR`
- Repro:
  `fd1 = open("/log", O_WRONLY|O_CREAT|O_APPEND); fd2 = open("/log", O_WRONLY|O_APPEND);` — both snapshot the same size at open, then writes through each clobber the other.
- Remediation: Make append atomic: resolve the write offset from the current inode size inside the same lock that performs the write, which is what POSIX requires and what Linux does in `generic_file_write_iter`.

### SLOPOS-2026-0028
- Title: `stat`, `fstat` and `sys_info` copy uninitialized struct padding to userland
- Status: open
- Confidence: 82 — evidence 38 (padding confirmed present in the shipped kernel.elf, and the copy-out path read), exploitability 22 (one syscall, but the leak is a few bytes of the calling task's own kernel stack), reproducibility 22 (deterministic that padding leaks; the content varies)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N` — **3.3 LOW**
- Impact: `UserFsStat` leaks 3 bytes and `UserSysInfo` 8 bytes of uninitialized `#[repr(C)]` padding from the calling task's kernel stack. Small, but it is a repeatable kernel-memory disclosure primitive and the classic seed for defeating layout randomisation.
- Evidence:
  - abi/src/fs.rs:79-86 — `#[repr(C)] pub struct UserFsStat { pub type_: u8, pub size: u32 }` — u8 at offset 0, u32 must be 4-aligned, so offsets 1..4 are implicit padding and `size_of` is 8
  - core/src/syscall/fs/fd_handlers.rs:65-70 — `let mut stat = UserFsStat { type_: 0, size: 0 }; ... copy_to_user(out.inner(), &stat)` — a field-wise struct literal, which leaves padding uninitialized per the Rust abstract machine
  - core/src/syscall/fs/path_handlers.rs:81-86 — the same pattern in `syscall_fs_stat`
  - slopos-ostd/src/user/copy.rs:252-267 — `copy_value_to_user` computes `let len = core::mem::size_of::<T>()` and `rep movsb`s that many bytes from `value as *const T as *const u8`; it copies the padding, it does not know about fields
  - abi/src/syscall/types.rs:20-35 — `UserSysInfo` is u32×5 then u64×3 then u32×2 then i64 then u32: 4 bytes of implicit padding at offset 20 (before the first u64) plus 4 bytes of tail padding, none of it named by a `_pad` field
- Repro:
  Pre-fill the destination buffer with a sentinel, call `syscall(100, fd, &st)`, and observe that the padding bytes are not the sentinel.
- Remediation: Zero the struct before populating it — `let mut st: UserFsStat = Zeroable::zeroed();` — or make these ABI types `Pod` with explicit reserved fields rather than implicit padding. Linux uses `memset` plus explicit `__pad` members for the same reason.

### SLOPOS-2026-0029
- Title: `klog` has no rate limiting and userland can drive it from a cli-held lock
- Status: open
- Confidence: 90 — evidence 38 (the userland-reachable log sites and the polled-UART write path read directly), exploitability 28 (a one-line loop), reproducibility 26 (deterministic; severity scales with the serial baud rate)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5 MEDIUM**
- Impact: Userland-reachable `klog_info!` sites — an unknown syscall number is one — emit a full line byte-at-a-time through a polled UART while holding a cli-held ticket lock. There is no rate limiting of any kind. A tight loop from any process monopolises the log lock and stalls every CPU that touches it, for as long as the loop runs.
- Evidence:
  - slopos-ostd/src/klog.rs:60-64 — `static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(KlogLevel::Info as u8)`; `is_enabled` is a bare level comparison — no token bucket, no per-site suppression, no `_once` variant anywhere in the tree (grep for `ratelimit|rate_limit|printk_once` finds only `fblog.rs:181`)
  - drivers/src/serial.rs:78-99 — `with_klog_lock` does `cpu::save_flags_cli()`, spins for a ticket, runs the whole formatted line, and only then restores flags: the entire line is emitted with IRQs disabled
  - slopos-ostd/src/early_console.rs:46-66 — `write_byte` polls `UART_LSR_TX_EMPTY` in a `spin_loop` per byte
  - drivers/src/serial.rs:265-269 — DLAB divisor is written as `0x01`, i.e. 115200 baud → ~87 µs per byte
  - core/src/syscall/dispatch.rs:58-61 — `klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno)` on every unrecognised syscall number
- Repro:
  `for (;;) syscall(999);` — each iteration emits a ~40-byte line through the polled UART under the lock.
- Remediation: Add printk-style rate limiting (`printk_ratelimited` / `DEFINE_RATELIMIT_STATE` is the reference), demote userland-triggerable messages below the default level, and move the UART write out of the locked region into a buffered emitter.

### SLOPOS-2026-0030
- Title: Ready-queue selection is strict priority with no aging backstop
- Status: open
- Confidence: 84 — evidence 38 (the dequeue order and the absence of any boost/decay path read directly), exploitability 22 (a spin loop at the caller's own tier, now capped at Normal), reproducibility 24 (deterministic starvation of Low by Normal)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5 MEDIUM**
- Impact: Selection is strict fixed priority with FIFO within a tier and no aging backstop anywhere in the scheduler. A `Normal` task that never blocks starves every `Low` task indefinitely; nothing raises a starved task's effective priority over time. The escalation half of this finding is closed — `syscall_spawn_path` now admits only `Normal` and `Low`, so `High` and `Idle` are no longer user-requestable — but the missing backstop is not.
- Evidence:
  - sched/src/per_cpu.rs:404-420 — `dequeue_highest_priority` is `for queue in &self.ready_queues { if let Some(task) = queue.dequeue() { return Some(task) } }`: a linear scan of priority levels 0..4, first non-empty wins, unconditionally
  - abi/src/task.rs:184-199 — `High = 0, KernelIo = 1, Normal = 2, Low = 3, Idle = 4`; the doc on KernelIo says "reserved for paths whose progress is required for correctness (delivering packets, draining TX rings, firing TCP retransmit timers)"
  - sched/src/task/task_lifecycle.rs:631 — `task_ref.priority = TaskPriority::from_u8(priority);` is the only write to `priority`; no boost, decay or aging path exists anywhere in the crate
- Repro:
  Spawn a `loop {}` binary at `TaskPriority::Normal` via `spawn_path`, then spawn anything at `Low`. The `Low` task never runs.
- Remediation: Add an aging or bandwidth backstop so no tier can starve indefinitely — EEVDF's lag accounting is the reference. The tier-restriction half is done.

### SLOPOS-2026-0031
- Title: Futex buckets cap waiters at 16 and return ENOMEM, which every userland futex wrapper discards
- Status: open
- Confidence: 86 — evidence 38 (the bucket cap, the error return and the slibc discard all read directly), exploitability 24 (18 contending threads), reproducibility 24 (deterministic once the bucket fills)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5 MEDIUM**
- Impact: Buckets hold at most 16 waiters, globally across processes and keyed address-agnostically, and return ENOMEM beyond that. slibc's mutex and condvar loops discard the error and retry, so the 17th waiter on a contended lock becomes a full-core busy-spin instead of blocking.
- Evidence:
  - sched/src/futex.rs:20-23 — `const FUTEX_HASH_BUCKETS: usize = 64;` and `const FUTEX_MAX_WAITERS_PER_BUCKET: usize = 16;`
  - sched/src/futex.rs:55-58 — `struct FutexBucket { waiters: [FutexWaiter; FUTEX_MAX_WAITERS_PER_BUCKET], count: usize }` — a fixed array, not a list
  - sched/src/futex.rs:144-154 — the free-slot search; `let Some(idx) = slot_idx else { return slopos_abi::syscall::ERRNO_ENOMEM as i64; }`
  - slibc/src/thread/mutex.rs:67-73 — `loop { let old = state.swap(2, Acquire); if old == 0 { return 0; } let _ = Sys::futex_wait(state.as_ptr() as *const u32, 2, 0); }` — the return value is discarded with `let _ =`
  - slibc/src/thread/condvar.rs:56 and rwlock.rs:54,93 — same `let _ = Sys::futex_wait(...)` shape
- Repro:
  Spawn 18 threads contending one `pthread_mutex_t` (or 17 in `pthread_cond_wait` on one condvar) whose futex word hashes to a single bucket. CPU usage pins at 100% with no progress.
- Remediation: Make the waiter list per-key and unbounded (an intrusive list off the futex key costs no allocation), and make the userland wrappers treat an unexpected errno as fatal rather than retrying. Linux's futex hash buckets hold an intrusive `plist` with no cap for exactly this reason.

### SLOPOS-2026-0032
- Title: `FUTEX_WAIT` accepts a timeout and silently ignores it
- Status: open
- Confidence: 90 — evidence 40 (the timeout argument and the untimed park path both read directly), exploitability 24 (one syscall), reproducibility 26 (deterministic — the wait never returns)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L` — **3.3 LOW**
- Impact: The waiter always parks through the untimed path, so a timed wait never returns ETIMEDOUT and blocks forever. Any userland code that relies on a bounded futex wait — `pthread_cond_timedwait`, timed lock acquisition, watchdog patterns — hangs instead of timing out.
- Evidence:
  - core/src/syscall/process_handlers.rs:698-711 — `define_syscall!(syscall_futex (ctx, uaddr: u64, op: u64, val: u32, timeout: u64) ... FUTEX_WAIT => slopos_sched::futex::futex_wait(uaddr, val, timeout)`
  - sched/src/futex.rs:106 — `pub fn futex_wait(uaddr: u64, expected: u32, _timeout_ms: u64) -> i64` — the parameter is bound to `_timeout_ms` and never read in the body (:106-190)
  - sched/src/futex.rs:103-105 — "The timeout parameter is currently accepted but not enforced (always waits indefinitely). This matches the rollback plan in the task description."
  - sched/src/futex.rs:185-189 — `if blocked { yield_blocked_task(); }` — the untimed primitive, not `yield_blocked_task_with_timeout`
  - sched/src/scheduler.rs:1736-1749 — `yield_blocked_task_with_timeout(timeout_ms)` exists and does exactly what is needed
- Repro:
  Write V to an aligned user word, then `syscall(SYSCALL_FUTEX, uaddr, FUTEX_WAIT, V, timeout=50)` with no other thread waking it. The call never returns.
- Remediation: Route the timeout through the existing timed-sleep path, or reject a non-zero timeout with ENOSYS until it is implemented — silently ignoring it is the one option that cannot be detected by a caller.

### SLOPOS-2026-0033
- Title: `synchronize_rcu` allocates infallibly and is `call_rcu`'s own out-of-memory fallback
- Status: open
- Confidence: 87 — evidence 38 (the `.expect` and the fallback path read directly), exploitability 22 (requires prior heap exhaustion), reproducibility 27 (deterministic once the heap is exhausted)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:N/I:N/A:H` — **4.7 MEDIUM**
- Impact: `synchronize_rcu` allocates a per-CPU snapshot vector with `.expect`, and it is also what `call_rcu` falls back to when it cannot allocate a callback node. On a machine with three or more CPUs, a heap-exhausted deferred free therefore panics the kernel rather than degrading — the fallback path allocates.
- Evidence:
  - slopos-ostd/src/sync/rcu.rs:255 — `let mut snaps = KVec::<u64>::zeroed(n).expect("rcu: snaps alloc");` inside `synchronize_rcu`; `n` scales with CPU count, and `call_rcu` falls back to `synchronize_rcu` when its own node allocation fails
- Repro:
  Drive the kernel heap to exhaustion (anonymous faulting or task spawning until a slab refill fails), then have any task call `synchronize_rcu` or trigger a `call_rcu` whose node allocation fails.
- Remediation: Make the snapshot allocation-free by using a fixed per-CPU array sized by MAX_CPUS, so the OOM fallback path cannot itself allocate. This is the general rule for a reclaim path: it must not require the resource it exists to reclaim.

### SLOPOS-2026-0036
- Title: A malformed compositor frame wedges that client's connection permanently
- Status: open
- Confidence: 88 — evidence 38 (the decode path and the absence of an error reply read directly), exploitability 26 (four bytes on a connected socket), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:L` — **3.3 LOW**
- Impact: An undecodable length prefix leaves the client's connection never serviced again, with no `Event::Error` returned and its windows left on screen. The slot and surfaces are reclaimed when the fd closes, so this is a wedge rather than a permanent leak — but a client that keeps the fd open holds a slot out of 32 and keeps stale windows composited.
- Evidence:
  - slop-protocol/src/connection.rs:277-300 — `try_decode` reads the 4-byte length prefix, and on `payload_len > MAX_MSG_SIZE` (8192) returns `Err(ProtocolError::MalformedMessage)` at :287-289 *without advancing `self.read_pos`*. The same happens for any unrecognized tag, since `Request::decode`'s fallthrough at slop-protocol/src/codec.rs:515 returns `MalformedMessage`.
  - userland/src/apps/compositor/protocol.rs:344-360 — `process_client`'s loop maps `Err(ProtocolError::Disconnected)` to teardown but every other error to a bare `break`, then `return true` ('still connected'). No `Event::Error` is sent and `cleanup_client` is not called.
  - slop-protocol/src/connection.rs:186-195 — `recv` calls `try_decode` *first* and `?`-propagates, so once the buffer head is undecodable `try_fill_buf` is never reached again on this path; the bad prefix is re-parsed forever.
  - slop-protocol/src/server.rs:305-317 — `probe_disconnected` only flags the client when `try_fill_buf` returns `Err(Disconnected)`; a wedged-but-open socket returns `Err(BufferFull)` instead (connection.rs:305-310), so the per-frame `cleanup_disconnected` sweep (userland/src/apps/compositor/mod.rs:833-836) never reaps it.
  - ring/src/enter.rs:742-767 — `harvest_poll_multishot` is edge-triggered (`if ready != 0 && ready != row.last_revents`), so the per-client task parks forever at `stream.next().await` rather than spinning; the slot simply never comes back.
- Repro:
  Connect to the compositor's AF_UNIX socket, complete the Hello handshake, map a window, then write four bytes `FF FF FF FF`. The connection is never serviced again and the window remains on screen.
- Remediation: On a decode failure, send `Event::Error` and close the connection — a protocol violation should be fatal to the connection, as it is in Wayland (`wl_display.error` followed by disconnect).

### SLOPOS-2026-0037
- Title: The compositor clipboard has no authorization
- Status: open
- Confidence: 85 — evidence 38 (the request handler read; no focus, surface or serial gate present), exploitability 26 (connect and ask), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N` — **7.1 HIGH**
- Impact: Any process that can connect to `/run/compositor` can read or replace the clipboard with no surface, no focus and no serial. In a system with no uid model this means any process reads whatever the user last copied — passwords included — and can substitute content into any paste.
- Evidence:
  - userland/src/apps/compositor/protocol.rs:781-788 — `handle_clipboard_paste` immediately queues `Event::PasteReady { len: self.clipboard.len }` to whichever client asked, with no check of focus, pointer, or serial.
  - userland/src/apps/compositor/protocol.rs:793-819 — `handle_clipboard_read` copies the clipboard into any client-supplied memfd, again with no authorization check.
  - userland/src/apps/compositor/protocol.rs:753-776 — `handle_clipboard_copy` replaces the global clipboard from any client at any time, with no serial.
  - userland/src/apps/compositor/protocol.rs:442-450 — `handle_request` dispatches all three unconditionally for any `client_idx`.
  - Contrast userland/src/apps/compositor/protocol.rs:633-650 — `handle_set_cursor_shape` in the *same file* is correctly gated on `s.has_pointer && serial == s.last_enter_serial`, with a doc comment explaining exactly why ('so no surface can influence the cursor unless the pointer is over it'). The clipboard paths have no equivalent.
- Repro:
  `socket(AF_UNIX, SOCK_STREAM)` + `connect("/run/compositor")`, complete the handshake, then issue a clipboard read or write request with no surface.
- Remediation: Gate clipboard access on the requesting client holding keyboard focus and presenting a valid input serial, which is the Wayland `wl_data_device` model (`set_selection` requires a serial from a recent input event).

### SLOPOS-2026-0038
- Title: Runtime display mode-set frees the old scanout while the vconsole still points at it
- Status: open
- Confidence: 80 — evidence 36 (the free and the retained cached base/pitch both read directly), exploitability 22 (needs a runtime mode-set, which is reachable but not a bare syscall), reproducibility 22 (the write-after-free happens on the next vconsole blit, which may be much later)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:N/I:N/A:H` — **4.7 MEDIUM**
- Impact: A runtime mode-set returns the old scanout backing to the buddy allocator while the vconsole keeps its cached base and pitch pointing at those pages. A later vconsole blit — crash-recovery restore or a panic screen — writes into memory that now belongs to something else. The panic path is exactly when this is least tolerable.
- Evidence:
  - core/src/syscall/ui_handlers.rs:276-285 — `syscall_set_display_mode` is a live syscall (`requires(compositor)`) that calls `video::set_display_mode(width, height)`
  - video/src/lib.rs:107-115 — `video_set_display_mode` calls `(g.set_mode)(w,h)` and then *only* `framebuffer::init_with_display_info(fb.address, &fb.info)`
  - video/src/lib.rs:238-243,254 — the full adoption path `install_scanout_provider` additionally calls `vconsole::register_framebuffer(base, pitch, width, height, bpp)` and `mouse::set_bounds(..)` and `scanout::set_current_framebuffer(ctx.fb)`; none of those run on the mode-set path
  - drivers/src/virtio_gpu/mod.rs:1013-1019 — `set_mode` does `resource_unref(old.resource_id)` and `free_page_frame(old.backing_phys)`, returning the old scanout pages to the buddy allocator
  - drivers/src/tty/vconsole.rs:1648-1696 — `register_framebuffer` is the only writer of `VCONSOLE_STATE.fb.base/pitch/width/height` and of the shadow buffer sizing
- Repro:
  Boot with the virtio display so the vconsole is registered on the GPU backing, trigger a runtime mode-set, then force a vconsole write (a kernel log line on the framebuffer console, or a panic).
- Remediation: Re-point the vconsole at the new scanout before freeing the old backing, and make the scanout provider registration hold a reference so the pages cannot be freed while a consumer is registered.

### SLOPOS-2026-0039
- Title: Device-supplied PCI offsets are used to map MMIO without bounding them against the BAR
- Status: open
- Confidence: 85 — evidence 38 (all three offsets traced from config space to the mapping/index with no bound check), exploitability 24 (requires a malicious PCIe function — Thunderbolt or a modified device model — not a guest-side trigger), reproducibility 23 (deterministic given control of config space)
- CVSS vector/score: `CVSS:3.1/AV:P/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H` — **7.6 HIGH**
- Impact: The virtio capability offset and length, the MSI-X table and PBA offsets, and the virtqueue notify offset are all read from device config space and used to map or index MMIO with no check against the probed BAR size. A hostile PCIe function — plausible on a laptop with Thunderbolt — turns this into an arbitrary MMIO mapping, and one path turns it into a kernel panic on the TX hot path.
- Evidence:
  - drivers/src/virtio/pci.rs:41-51 — `map_cap_region(info, bar, offset, length)` computes `bar_info.base.wrapping_add(offset as u64)` and maps `length` bytes; `bar_info.size` is never consulted. `offset` and `length` are read from the device's vendor capability at :76-77
  - drivers/src/msix.rs:296-298 — `table_phys = table_bar.base.wrapping_add(cap.table_offset as u64)`, `table_bytes = cap.table_size * 16`; both come from device config space (:246-255) and neither is bounded by `table_bar.size`. Same at :311-313 for the PBA
  - drivers/src/virtio/queue.rs:299-307 — `notify_queue` computes `offset = queue.notify_off * notify_off_multiplier` and calls `notify_cfg.write::<u16>(offset, ..)`; `notify_off` is read from the device at queue.rs:284, the multiplier from config space at virtio/pci.rs:86
  - slopos-ostd/src/mm/io_mem.rs:550-561 — `IoMem::write` *asserts* on out-of-bounds ('driver-side miscoding is unrecoverable'), i.e. panics
  - mm/src/mmio.rs:46-52 — `MmioRegionExt::map` calls `register_io_mem_range(PhysRange{base, len})` unconditionally before `IoMemRegistry::reserve`, so the OSTD insensitive-range gate accepts whatever the caller asks for
- Repro:
  Present a PCI function whose virtio capability declares an offset beyond its BAR. Requires control of the device, not of the guest.
- Remediation: Validate every device-supplied offset and length against the probed BAR size before mapping or indexing, and reject the device otherwise. This is what `pci_iomap_range` bounds-checking gives Linux drivers for free.

### SLOPOS-2026-0040
- Title: virtio-net's RX ring shrinks monotonically and never refills
- Status: open
- Confidence: 85 — evidence 38 (the repost path confirmed to run only at probe, and the failure branch read), exploitability 24 (needs one page-allocation failure under memory pressure, which a remote peer can help induce), reproducibility 23 (depends on hitting the allocation failure)
- CVSS vector/score: `CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:N/A:H` — **5.9 MEDIUM**
- Impact: A failed page allocation permanently retires an RX descriptor; the only repost pass runs once at probe. Under memory pressure the receive ring drains one descriptor at a time and never recovers, degrading to no receive capability at all. Networking is lost until reboot.
- Evidence:
  - drivers/src/virtio_net.rs:835 `virtnet_prepost_rx_buffers` — the only RX fill pass; its sole caller is the init path at :1496, so a descriptor whose buffer allocation fails is never re-offered
- Repro:
  Drive the kernel into memory pressure while receiving traffic so an RX buffer allocation fails. Each failure permanently removes a descriptor.
- Remediation: Repost RX buffers on every completion, and retry failed allocations on the next NAPI poll instead of retiring the descriptor. Linux's `virtnet_receive` refills in the poll loop and schedules a delayed refill when allocation fails.


### SLOPOS-2026-0042
- Title: `kernel_reboot` never flushes the filesystem (`kernel_shutdown` does), and the ext2 image carries no dirty-state word
- Status: open
- Confidence: 90 — evidence 38 (the shutdown path read; no sync call present, no dirty flag in the superblock write path), exploitability 24 (an ordinary reboot), reproducibility 28 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:L` — **4.4 MEDIUM**
- Impact: `kernel_reboot` discards write-back data, and because the image carries no dirty-state word it is left claiming to be clean, so a subsequent fsck sees a consistent-looking filesystem and does not repair the loss. Note the asymmetry: `kernel_shutdown` flushes correctly and its comment explains why the flush must precede `disable_interrupts` (the virtio-blk completion path needs IRQs and the scheduler). `kernel_reboot` simply omits that call.
- Evidence:
  - boot/src/shutdown.rs:145 — `kernel_shutdown` calls `flush_filesystems_for_shutdown()` before `disable_interrupts`, with a written rationale about IRQs being load-bearing for durability
  - boot/src/shutdown.rs:229-250 — `kernel_reboot` goes `ensure_shutdown_mmio_mapped()` -> `disable_interrupts()` -> `kernel_quiesce_interrupts()` -> the `REBOOT_METHODS` table; `flush_filesystems_for_shutdown` is never called
  - boot/src/shutdown.rs:31-37 — the helper both paths would share: `if !FS_SYNCED.enter() { return; } ... slopos_fs::ext2_vfs_shutdown_sync();`
  - fs/src/ext2/ondisk.rs — `Superblock` carries no `s_state` field and `Superblock::parse` never reads one, so a mounted image is never marked not-clean
- Repro:
  Write a file to an ext2-backed mount, then `reboot` (not `halt`). The data is gone and the image still reports clean. The same sequence ending in `halt` persists correctly, which is the diagnostic that isolates it to `kernel_reboot`.
- Remediation: Call `flush_filesystems_for_shutdown()` from `kernel_reboot` at the same point `kernel_shutdown` does — before `disable_interrupts`, for the reason its comment already gives. Separately, add `s_state` to the superblock, set the not-clean bit on mount and clear it on clean unmount, which is the mechanism fsck relies on to know it must run.

### SLOPOS-2026-0043
- Title: ext2 mounts and writes any image whose magic and geometry are sane, with no feature-compatibility gate
- Status: open
- Confidence: 85 — evidence 38 (the mount path read; no `s_feature_incompat` check present), exploitability 24 (requires the user to mount an untrusted image), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:R/S:U/C:N/I:H/A:H` — **6.6 MEDIUM**
- Impact: There is no `s_feature_incompat`/`s_feature_ro_compat` check and no read-only fallback, so SlopOS mounts read-write a filesystem whose layout it cannot represent — extents, 64-bit block numbers, metadata checksums — and writes to it. The result is an image that other implementations then read as corrupt.
- Evidence:
  - fs/src/ext2/ondisk.rs — `Superblock` has fields for magic, rev_level, first_ino, inode_size and geometry only; `Superblock::parse` validates magic, block/inode geometry and non-zero divisors, and reads no `s_feature_incompat` / `s_feature_ro_compat` at all
- Repro:
  Mount an ext4 image with extents enabled. It mounts read-write, and any write corrupts it.
- Remediation: Check `s_feature_incompat` against the set actually implemented and refuse to mount otherwise; check `s_feature_ro_compat` and mount read-only when an unsupported read-only-compatible feature is present. This is exactly the gate Linux's `ext4_feature_set_ok` performs.

### SLOPOS-2026-0049
- Title: `halt` and `reboot` have no authorization check, by three separate paths
- Status: open
- Confidence: 92 — evidence 38 (all three handlers read; two carry no `requires` clause and the third carries only an existence check), exploitability 28 (one syscall), reproducibility 26 (deterministic)
- CVSS vector/score: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5 MEDIUM**
- Impact: One instruction from any task powers off or reboots the machine. The third path matters because a slot-level authorization gate would leave it green: an unprivileged, retryable syscall reaches the reboot primitive two calls away from the one being audited.
- Evidence:
  - core/src/syscall/core_handlers.rs:57 — `syscall_halt` carries no `requires` clause.
  - core/src/syscall/core_handlers.rs:63 — `syscall_reboot` carries no `requires` clause.
  - core/src/syscall/ui_handlers.rs:312-333 — `syscall_roulette_result` carries only `requires(task_id: task_id)` and calls `platform::kernel_reboot` on its loss arm; `roulette`/`roulette_result` are unprivileged and retryable.
  - kernel-services/src/platform.rs:57,62 — the terminal primitives are function-pointer indirections in a peer service crate of `core`, so no type-level obligation can currently be attached to them.
- Repro:
  `syscall_halt()` from the shell. Or `roulette_spin()` followed by `roulette_result()` until the loss arm is taken.
- Remediation: A `Power` capability on `halt` and `reboot`, with the terminal primitives moved into `slopos-ostd` behind a witness so an unchecked call site does not compile; the roulette loss arm additionally gated on a boot-mask bit in the idiom `syscall_test_panic` already uses (core/src/syscall/test_handlers.rs:123-129). A reachability gate covers the kernel-initiated callers. See `plans/authority-model.md` phase 3.

## Relevant NVD CVE Analogs (fetched)

Retrieved using NVD API pattern:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Selected analogs:

| CVE | Vector | Score | Severity | Why relevant |
|---|---|---:|---|---|
| CVE-2016-5696 | CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:L/I:L/A:N | 4.8 | MEDIUM | Global challenge-ACK counter side channel — SLOPOS-2026-0015 is the same design |
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | Filesystem metadata parsing / ext* class |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening analog |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in kernel path |

## Priority Remediation Plan

Ordered by what removes the most exposure per unit of work, not by score.

1. **Validate the signal-return XSAVE area** (0007). An unprivileged kernel-halt primitive: the `xrstor64` in `rt_sigreturn` restores a user-supplied image with no header validation and no #GP fixup.
2. **Reseed the DNS resolver and validate response provenance** (0012), then replace the ISN generator with a keyed PRF (0014) and add the RFC 793 §3.9 acceptability gate (0013). These three are the network-facing set and share a test harness.
3. **The TLB correctness set** (0017, 0018, 0019). Stale writable translations across address spaces are the only findings here that could become memory corruption rather than denial of service.
4. **The filesystem integrity set** (0021–0027, 0042, 0043). Individually low-scoring, collectively the reason the filesystem cannot yet be trusted with data that matters.
5. **The residue of resource accounting** (0016, 0030, 0031). Per-principal accounting has landed in full and 0035 went with it — a ring is a `FileBacking`, so it now carries a per-process `ObjectRow` charge. These three do **not** fall out of it, and grouping them under it was the error: a per-principal count bounds how many of a thing one process holds, and none of these is that shape. 0016 is a reference *cycle*, which no count collects; its fix is a type restriction on what may be passed over a socket, the same place io_uring landed after five years. 0030 is a scheduling-fairness gap, not a capacity one. 0031 is a shared *namespace* — fill one futex bucket and every other process whose word hashes there is denied — which per-principal partitioning would fix and per-principal counting would not — the accounting design named this and put it out of scope deliberately.
6. **Authorization** (0049, and the structural halves of the fixed authorization findings). The relation checks have landed; the capability set, the witness that makes a missing check a compile error, and the display and input seats are `plans/authority-model.md`, which depends on `plans/process-object.md`.
