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

The 2026-08-24 pass took the four remaining scored findings — 0016, 0017, 0020
and 0030 — plus the 0006 residual, researched each against its upstream
reference, and **all four scored entries are removed as fixed.** Each fix is
pinned by a test that was confirmed to fail on the unfixed tree, by reverting
the fix and re-running rather than by assertion.

- **0017 (PCID reuse, 7.8 HIGH).** The wrapping 12-bit counter and the
  unconditional NOFLUSH are both gone. `VmSpace` no longer carries a `pcid`
  field at all; the tag and the right to skip the flush are one decision, taken
  by the per-CPU pool through a new `CursorUnmapHook::select_cr3`. That pool —
  the Linux-style `tlb_state`/generation scheme the ledger noted was sitting
  beside the bug as dead code — is now the live path, and OSTD falls back to a
  flushing PCID-0 load when there is no pool or PCIDE is off. The dead code
  being wired up is the fix, which is why the entry is closed rather than
  narrowed.
- **0020 (`execve` is not an address-space boundary, 7.1 HIGH).**
  `process_vm_reset_for_exec` severs every user mapping — heap, mmap arena,
  shared memfds, rings — through each backing's own unmap path, so mapcounts
  and page charges settle correctly, then re-seeds a freshly randomised layout
  from one shared definition. It runs *before* the new image is loaded, so a
  failure is still recoverable.
- **0016 (SCM_RIGHTS cycle, 5.5 MEDIUM).** Closed by a type, not a collector.
  `FdContainment` is a total function over `FileKind`, and `unix_sendmsg` takes
  `LeafFileRef` — a witness whose only constructor checks containment — so a
  description that owns descriptions cannot enter an ancillary queue. This is
  io_uring's settled answer generalised. **The ledger's proposed remediation
  would have been insufficient:** refusing AF_UNIX sockets leaves
  `FileKind::Ring` open, whose in-flight rows own a `FileRef` each and close
  exactly the same cycle.
- **0030 (no aging backstop, 5.5 MEDIUM).** A per-tier passed-over count: a
  non-empty tier skipped `AGING_THRESHOLD` times is served once. An EEVDF
  calendar wheel was implemented first and **abandoned** — it destabilised
  signal delivery under load (`utest_ctrlc_flood`), because expressing
  "`KernelIo` must pre-empt `Normal`, a TX ring is draining" inside one
  lag-ordered pool needs weights so extreme the fair ordering is decorative.
  The tiers here are correctness statements, not preferences. The backstop
  leaves them intact and bounds what they cost everyone below.

Three notes on what the pass found that the ledger had wrong. **0006's headline
claim was false** — the slice it named is bounded by construction for every
mountable block size — and the real defects at that site were different and
worse; see the entry. **The gates caught what review did not**, again:
`check_stack_sizes.sh` rejected two ext2 tests that held two mounts in one
frame after `Ext2Fs` grew a geometry field, fixed by splitting the frames
rather than widening the allowlist. And **CR3 bit 63 is write-only**, so the
NOFLUSH decision cannot be asserted by reading the register back; that property
is pinned against the pool instead, which is where it is decided.

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

*(No findings currently meet the confidence >= 80 bar. SLOPOS-2026-0006 remains
open below at confidence 60 and is therefore not CVSS-scored.)*

## Open SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0006
- Title: ext2 group-descriptor and directory-record trust on a malformed image
- Status: open
- Confidence: 60 — evidence 30, exploitability 12, reproducibility 18. Unchanged: there is no `mount(2)`, so feeding a crafted image requires control of the VM's disk. Below the 80 threshold for a guaranteed CVSS-scored issue.
- Status note (2026-08-24): **the arms this ledger described are closed, and the ledger's headline claim was wrong.** The slice it named cannot go out of bounds: `within = (group % desc_per_block) * 32` is bounded by `desc_per_block * 32`, which equals `block_size` exactly for every admissible block size (1024/2048/4096), and `CachedBlock::data()` is exactly `block_size` long. What *was* wrong at that site is now fixed — an unbounded `block_idx`, a `groups_count()` computed by overflowing arithmetic, and descriptor contents (`inode_table`, `block_bitmap`) trusted as block pointers that `write_inode_num` then wrote through. `Ext2Geometry` (`fs/src/ext2/geometry.rs`) validates the whole superblock geometry once at mount and is the only constructor of `GroupIdx`, so an unbounded group index is no longer representable. Four real arithmetic panics found alongside it (`groups_count` overflow, two allocator multiplications, and a directory `rec_len - actual_size` underflow reachable from an ordinary `create_file`) are fixed and pinned.
- Evidence: `fs/src/ext2/geometry.rs` `Ext2Geometry::derive` (checks G1–G11), `validate_desc`; `fs/src/ext2/types.rs` `GroupIdx` private field; `fs/src/ext2/dir.rs` `parse_record`, the single record predicate the walker and the inserter now share.
- Impact: malformed-image-triggered panic (DoS) or, in release builds where overflow checks are off, silent metadata type confusion. Because `fs/` is `#![forbid(unsafe_code)]`, never memory unsafety/UB.
- CVSS vector/score: not assigned because confidence is below 80.
- Residual: the ext2 code still trusts an image's *self-consistency* beyond the geometry — a descriptor whose free-count disagrees with its bitmap, or a directory whose `..` points elsewhere, is accepted. That is an fsck's job, not a bound's, and it is a correctness question rather than a safety one now that every derived index is checked.

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

Only one finding remains open, and it is deliberately unscored.

1. **The ext2 self-consistency residual** (0006). Every *derived* index is now
   bounded and every geometry field validated at mount, so what is left is an
   image that is internally inconsistent rather than out of range — a free
   count that disagrees with its bitmap, a `..` that points elsewhere. That is
   an fsck's job. It matters more if `mount(2)` ever lands, because today the
   only mount is boot-time from `disk0`, and feeding it a crafted image already
   requires control of the VM's disk.

The structural work the closed findings leave behind, worth stating because it
is what the next finding in each area will meet:

- `Ext2Geometry` is the only constructor of `GroupIdx`, so a new ext2 caller
  inherits the bounds rather than re-deriving them.
- `FdContainment` is total over `FileKind`, so a new descriptor kind must state
  whether it can own descriptions before it compiles.
- `CursorUnmapHook::select_cr3` is the only path to a PCID, so a future KPTI
  or ASID change has one place to change.
- `process_vm_reset_for_exec` and `create_process_vm_for` share one definition
  of a fresh address space, so a new region kind is added once.
