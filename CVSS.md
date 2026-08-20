# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17 (original); last reviewed 2026-08-12.
Method: repository-wide static review (`grep`, `ast-grep`, targeted source inspection), plus NVD CVE lookups via `curl` + `jq`. The 2026-07-30 pass added a structured sweep: six reference briefs on proven kernel designs, fifteen per-subsystem auditors, then one adversarial verifier per candidate finding (two independent lenses for the most severe), each instructed to default to REFUTED and to locate the guard that would disprove the claim. Only findings that survived verification with a concrete attacker trigger are recorded here. The 2026-08-08 pass swept the authorization and resource-accounting surfaces specifically, over twelve prior-art clusters and seven adversarial lenses, and added 0044–0052; its remediation lived in the process-object, resource-accounting and authority-model plans (all three since landed and retired); the nine findings that had no framework dependency have since been fixed and removed under the policy below.

The 2026-08-09 pass swept the resource-accounting implementation as it landed (phases 1-5). Per-principal ceilings are now enforced on seven kinds — descriptor slots, object rows, tasks, processes, in-flight `SCM_RIGHTS` custody, pinned pages, and kernel metadata (task stacks) — which closes the class of "one unprivileged process exhausts a fixed global table and every other process is permanently denied" for those resources. The sweep found one defect in the new code, in `Charge::try_extend`: a saturating add capped the token's amount below what the account row had already been debited, so the difference would never be refunded. Not recorded as a finding — it required a single object holding 4 billion units, which no ceiling permits, so there was no attacker-reachable trigger; fixed by refusing the extension instead, which makes the reservation's own `Drop` return the debit.

The same pass swept the two structural changes that landed alongside it. AF_INET sockets now hold their own wait queues on a pinned, boot-allocated spine rather than sharing one of 64 folded buckets, which removes a cross-socket wake amplification (one event woke up to sixteen unrelated sockets) without changing the correctness argument — the fallback path is chosen once by `OnceLock::call_once` and cannot flip under a parked task, so no wake can be lost. Input event-queue slots are now claimed only at a syscall or focus change, never from the PS/2 IRQ handler: a claim there acquired one of 32 slots on behalf of a task that never asked, at a point with no principal and no errno path, which is the confused-deputy shape at queue-registration scope.

**Both residual exposures the previous pass recorded are now closed.** Keepalive DMA frames carry their own independent `PinnedBytes` charge (`KeepaliveFrames`), refunded where the frames are actually released rather than at ring teardown, and a retransmit's second in-flight DMA is charged as the second pin it is. `Pages` is charged per address space and enforced at 65 536 pages, so address-space growth is no longer bounded only by the frame allocator.

The 2026-08-10 pass swept the second implementation round — `Pages`, kernel-heap backing, keepalive charges, the reclaim tier, and `prlimit64`. Three defects were found and fixed; none is recorded as a finding, because none had an attacker-reachable trigger, and the policy below forbids scoring speculative issues:

1. **`shrink_clean` rebuilt the block-cache index with an allocating `KBTreeMap::insert`**, on the path that runs *because* allocation failed. A failed re-insert would have left a live cached block unreachable under its own number — a correctness fault, not a safety one, and unreachable in practice because the reclaimer is only driven after an allocation has already failed and before any caller depends on the index. Replaced with an in-place repair of the single entry `swap_remove` moves, which allocates nothing.
2. **`prlimit64` mapped an unconvertible `rlim_cur` to the no-limit sentinel.** Had it been reachable this would have been an unprivileged escape from every ceiling: the widest possible `setrlimit` would have switched enforcement off for the caller's own account. It is not reachable — the `EPERM` check against the published hard limit runs first and bounds the value below `u32::MAX` for every published resource — so it is fixed as defence in depth (clamp to the current limit, never raise) and pinned by a userland test rather than scored.
3. **The `Pages` audit was vacuous as first written**, comparing two numbers maintained by the same code. Rewritten to compare the account row against the summed maps, which is the pair a phantom debit separates.

The reclaim tier was reviewed specifically for the deadlock class it could introduce, since it runs on allocation-failure paths reachable from under other subsystems' locks. Both reclaimers are non-blocking by construction: the quarantine reclaimer takes the buddy lock it already owns, and the ext2 reclaimer `try_lock`s a sleeping mutex held across block I/O and returns zero rather than waiting. Reclaim is never driven from `try_charge` — the arena takes no locks by construction, and a hook there would give it an inbound edge from every charge site at once — but from the caller of a refused allocation, at a syscall boundary or the demand-fault path, each with a single bounded retry.

The 2026-08-11 pass swept the authority model as it landed (phases 1-7): the seats, the syscall classification, the dispatcher check, `Power`, the capability promotions, `Signalable`, the exec intersection and the `Launch` bound. **SLOPOS-2026-0049 is closed and removed** — `halt` and `reboot` require `Power`, conferred by program identity on `/bin/halt` alone, and the `roulette_result` reachability arm is two-keyed on a boot flag that is clear under `tests=on`.

Four defects were found in the new code and fixed during the sweep; none is recorded as a finding, because none had an attacker-reachable trigger:

1. **`dup`/`dup2`/`dup3` bypassed the transferability predicate.** They alias through `FdEntry::try_alias` rather than `fileio_clone_file_ref`, so the first implementation left the cheapest duplication path open. A duplicated seat would produce a second holder the arbiter does not know about, so killing the holder could not reclaim the screen. Fixed by testing the predicate at both choke points; a planted defect fails exactly the `dup` and spawn-transfer test cases.
2. **`dispatch_handler` skipped the authority check.** The capability decision lives in the dispatcher, so a test calling a handler directly was asserting against a path userland cannot take — the keymap permission test proved the handler's own logic while claiming to prove reachability. `dispatch_entry` applies the real decision and the test now uses it.
3. **A dead `seat_resolve` helper.** Written for a descriptor-taking check the display syscalls do not perform (their ABI predates the seat and takes no fd), it would have drifted out of step with the helper actually used. Deleted rather than left unused.
4. **`exec` narrows `caps` but not `flags`.** `signal_dominates` reads the flag word, so a process deprivileged by exec keeps its old *signal* standing. The direction is defensive — the exec'd process stays protected from unprivileged peers rather than gaining the ability to signal them — so it is documented at the relation rather than changed: `caps` answers what may be invoked, `flags` answers who may be named, and converting the 146 flag readers to an atomic to unify them would be a large change in exchange for a strictly weaker guarantee.

The residual the model does not close is recorded honestly rather than scored: **SlopOS still authorizes integer arguments against a credential attached to the caller.** That is Linux's shape with enforcement Linux does not have — a `rustc` totality assert over the syscall table, a distribution ratchet, and a call-graph reachability gate — but it is not an object-capability system and no document in this tree claims otherwise. Related: SLOPOS-2026-0020 (`execve` is not an address-space boundary) is **unchanged and still open**; the authority model explicitly does not make exec a revocation point for memory, which is why every authority-bearing descriptor is close-on-exec.

The 2026-08-12 pass took the remediation itself rather than the sweep: twenty-two
findings were researched against their upstream reference (the RFC text, the
Linux commit, the Wayland protocol rule), fixed, and pinned by a test that fails
without the fix. **Twenty-four entries are removed** — 0012–0015, 0019, 0021–0029,
0031–0033, 0036–0040, 0042 and 0043. Two of those (0032, 0033) were **already
fixed in the tree and stale in this ledger**: the futex timeout is threaded
through to an absolute deadline that returns `ETIMEDOUT`, and `synchronize_rcu`'s
snapshot has become a `static [QsSlot; MAX_CPUS]`, so the out-of-memory fallback
allocates nothing. Both were re-verified twice before removal, since removal is
irreversible under the policy below.

The network set is the one worth reading as a set. The ISN generator's FNV chain
was closed under truncation, so the output depended only on the low 32 bits of
the boot secret; it is now SipHash-2-4 under a 128-bit CSPRNG key, per RFC 6528,
with the hash written from the published algorithm rather than taken from any
implementation and pinned by the paper's own test vectors. That fix is what makes
the others worth having: with a predictable ISN, the RFC 793 §3.9 acceptability
gate and the RFC 5961 §4 SYN mitigation are guessing games an attacker wins. Two
existing tests had to be **renamed and inverted** — they asserted that a SYN in
the synchronized state produces a RST, which is precisely the blind-reset
behaviour RFC 5961 exists to forbid.

Three defects were found in the new code by the gates rather than by review, and
are recorded because the gates are the reason they did not ship:

1. **`check_stack_sizes.sh` rejected `vfs::canon::canonicalise` at 2104 B** — a
   `[usize; MAX_PATH_LEN/2]` component-offset array. Fixed by narrowing the
   element type to `u16`, not by widening the allowlist; the same gate then
   caught a 3256 B case-table in the test that exercises it, fixed by moving the
   table to a `static`.
2. **`forbid(unsafe_code)` rejected the first futex rewrite.** Reading a parked
   task's futex word through `NonNull::as_ref` is `unsafe`, and `sched` may not
   contain it. The correct primitive already existed — `placement::with_parked_node`,
   the safe scoped borrow OSTD exports for exactly this — which is the framekernel
   discipline doing its job rather than obstructing it.
3. **The `UserFsStat`/`UserSysInfo` padding fix was enforced by the compiler.**
   Naming the holes as `_pad` members made every construction site a hard error
   until it accounted for them, and a `const` size assertion now fails the build
   if a future field reopens one.

What this pass explicitly does **not** close: 0016 (the SCM_RIGHTS reference
cycle, which needs a garbage collector or a type restriction, not a counter),
0017 (PCID reuse, which needs the dead ASID pool wired up), 0020 (`execve` is
still not an address-space boundary) and 0030 (the missing scheduler aging
backstop). 0006 is **partially** fixed and its confidence lowered to 60 — the
`inode_size` arm is closed and the group-descriptor arm is not.

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
| [SLOPOS-2026-0017](#slopos-2026-0017) | 7.8 | HIGH | PCIDs are assigned from a wrapping 12-bit counter with no reuse tracking while every CR3 write is NOFLUSH |
| [SLOPOS-2026-0020](#slopos-2026-0020) | 7.1 | HIGH | `execve` is not an address-space boundary |
| [SLOPOS-2026-0016](#slopos-2026-0016) | 5.5 | MEDIUM | AF_UNIX SCM_RIGHTS has no cycle policy, permanently leaking socket slots |
| [SLOPOS-2026-0030](#slopos-2026-0030) | 5.5 | MEDIUM | Ready-queue selection is strict priority with no aging backstop |

## Open SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0006
- Title: ext2 inode/group descriptor size trust can panic on out-of-bounds slicing
- Status: open
- Confidence: 60 — evidence 30, exploitability 12, reproducibility 18. Lowered from 70 on 2026-08-12: the `inode_size` half of the finding is closed, and what remains is the narrower group-descriptor claim. Below the 80 threshold for a guaranteed CVSS-scored issue.
- Status note (2026-08-12): **partially fixed.** `Superblock::parse` now rejects an `inode_size` that is not a power of two in `128..=block_size`, which is the arm that made the inode-table offset arithmetic address outside the block it had just read. Pinned by `test_ext2_inode_size_beyond_block_rejected`. The residual is the group-descriptor path: `GroupDesc::parse` still slices a fixed 32 bytes at `(group % desc_per_block) * 32` without asserting that the descriptor table's declared extent lies inside the block, so a crafted `blocks_per_group`/`groups_count` pair remains the open question.
- Evidence: `fs/src/ext2/mod.rs` `read_group_desc` computes `within = (group.raw() as usize % desc_per_block) * 32` and slices `&block.data()[within..within + 32]`; `fs/src/ext2/ondisk.rs` `GroupDesc::parse` trusts that slice's length.
- Impact: malformed-image-triggered out-of-bounds slice index. Because `fs/` is `#![forbid(unsafe_code)]`, this is a bounded-slice **panic** (DoS), never memory unsafety/UB.
- CVSS vector/score: not assigned because confidence is below 80.
- Remediation (proposed): bound each `within + size` against the containing block before slicing, and validate `groups_count()` against the descriptor table's actual extent. The adjacent feature-compatibility gate (formerly 0043) has landed.

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


## Relevant NVD CVE Analogs (fetched)

Retrieved using NVD API pattern:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Selected analogs:

| CVE | Vector | Score | Severity | Why relevant |
|---|---|---:|---|---|
| CVE-2016-5696 | CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:L/I:L/A:N | 4.8 | MEDIUM | Global challenge-ACK counter side channel — the design SLOPOS-2026-0015 had, now per-connection and jittered |
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | Filesystem metadata parsing / ext* class — the class SLOPOS-2026-0006's residual sits in |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening analog |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in kernel path |

## Priority Remediation Plan

Ordered by what removes the most exposure per unit of work, not by score.

1. **The TLB correctness set** (0017). `mprotect`'s missing shootdown (0019) has
   landed, which leaves PCID reuse as the only remaining way a stale translation
   crosses an address-space boundary — the one class here that could become
   memory corruption rather than denial of service. The fix is not a patch: the
   correct Linux-style ASID pool already sits beside the wrapping counter as
   dead code, and wiring it up means tracking which CPU last loaded each PCID
   and flushing on reuse.
2. **`execve` as an address-space boundary** (0020). Unchanged and still open.
   The authority model deliberately does not make exec a revocation point for
   memory, which is why every authority-bearing descriptor is close-on-exec;
   that is a mitigation, not a fix. `process_vm_reset_for_exec` reusing
   `teardown_inner_mappings` is the shape.
3. **The AF_UNIX SCM_RIGHTS cycle** (0016). Not a counting problem, which is why
   per-principal accounting did not close it: a reference cycle is collected by
   a collector or prevented by a type restriction, and refusing to pass an
   AF_UNIX socket over an AF_UNIX socket is the cheap half of what io_uring
   settled on after five years.
4. **Scheduler fairness** (0030). A `Normal` spin loop still starves `Low`
   indefinitely. EEVDF's lag accounting is the reference; the tier-restriction
   half is already done.
5. **The ext2 group-descriptor bound** (0006 residual). Small, and the adjacent
   feature-compatibility gate and `inode_size` bound have already landed, so
   this is finishing a job rather than starting one.
