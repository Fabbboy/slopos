# SlopOS Vulnerability Audit and CVSS Scoring

**No findings are open.** Last swept 2026-09-07 (Phase 7: the ext2 metadata
redo log, the bounded writeback pass, and the per-process disk-block quota —
the new hostile input at replay, the new userland-reachable log file, and what
"a ledger keyed on a principal" newly depends on). The two entries that sweep
and the one before it left open were resolved on 2026-09-08 and removed under
the policy below; what they taught is kept as method here.

Those two closures are each a class:

- **A refund must credit the principal that was charged, not the caller.**
  ext2's `free_block` refunded `geom.account()` — whoever ran the free — so a
  process at its `DiskBlocks` ceiling could unlink a file it did not create,
  watch its own row fall by that file's block count and allocate again, and
  the mirror case denied without malice: a process whose files someone else
  removed stayed charged for blocks that no longer existed. The ledger entry
  proposed a file-ownership model; that was the wrong layer. This kernel has
  no persistable principal at all (`getuid` is a literal zero, and an
  `AccountId` is a live slot index whose generations restart each boot, so
  writing one to an inode would name a stranger after a reboot), and an
  ownership *check* on the unlink would not have fixed the mirror case.
  Attribution did: `fs/src/ext2/blockcharge.rs` records, per mount, which
  principal was charged for an inode's blocks, and the free credits that
  record. The rule generalises past ext2 — **a ledger keyed on a principal
  needs a record of which principal, kept wherever the resource is, not
  inferred from whoever is running.**
- **A budget that is shared and charged to nobody is not a bound.** The
  file-mapping page sets had one system-wide ceiling of 16 inodes and 1024
  unreclaimable pages, summed across every slot and attributed to no account,
  so one process holding sixteen mapped inodes made file `mmap` and `msync`
  answer `ENOMEM` for every other process at no cost to its own accounted
  budget. Fixed by charging the frames to a principal on the `PinnedBytes`
  axis — a frame under a user PTE is pinned against reclaim, which is what
  that axis already counts — and by measuring a request against that
  principal's *share* of the registry rather than against the whole of it.

That sweep found eight defects. Seven were fixed inside the same unreleased
change and were therefore never ledger entries; the eighth was
`SLOPOS-2026-0055`, closed above. The Phase 6 sweep before it found six, of
which five were fixed the same way and the sixth was `SLOPOS-2026-0054`, also
closed above. Both sets of fixes are recorded here as method, because each is
a class rather than an instance:

- **A block number read off the medium is a write offset.** The log's replay
  bounds-checked every field except the one it multiplies into an offset, so a
  crafted or foreign `/.journal` turned a mount into an arbitrary write inside
  the partition — including past the filesystem extent, over the verity
  trailer that exists to detect it. Fixed by carrying the volume's extent into
  the log and refusing an out-of-range target at the scan, at the disposition
  and at the write.
- **A record that validates its geometry has not proved it is yours.** The log
  superblock checked magic, version, block size and capacity — all of which a
  log built for a different filesystem of the same shape also satisfies. Fixed
  by folding the log's inode, its first block and the volume's block count
  into the record and refusing a mismatch.
- **A refusal keyed on state is not a refusal keyed on identity.** `/.journal`
  was refused to readers only while a log was *attached*, so every mount that
  did not attach one served the metadata of every file an earlier boot
  changed. Fixed by resolving the path at mount whatever the outcome and
  refusing that inode unconditionally.

- **A new way to reach an object must re-ask the question the old way
  answered.** `mmap(MAP_SHARED, PROT_WRITE)` resolved a descriptor to
  `(fs, inode)` and checked only "regular file, within EOF" — not the
  descriptor's open mode, not `EXT2_IMMUTABLE_FL`, not `MOUNT_RDONLY`. Since
  the per-inode page set is the authority `read(2)` is routed through for
  every process, a sealed `/bin/halt` opened `O_RDONLY` could be rewritten in
  the view every other reader gets. Would have scored
  `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:H/A:N` — **5.5 MEDIUM** (the disk
  was saved only by `Ext2Fs::write_file` refusing an immutable inode, i.e. by
  two unrelated layers agreeing rather than by a rule). The fix carries the
  descriptor's `OpenMode` out of `fileio_get_open_file_handle` and refuses a
  writable shared mapping without it, with the seal re-checked inside
  `filemap::acquire` — and, from the second review round, `mprotect` refuses
  to widen a file mapping to writable at all, because the descriptor that
  authorised the mapping is not reachable from there.
- **"Read-only" is not "harmless".** `/dev/vd*` served raw device bytes to any
  process. SlopOS has no per-file read permission, so live file contents were
  not newly disclosed — but `unlink` is this kernel's only primitive for
  making something unreadable and ext2 does not zero freed blocks, so a
  process that never held a file open could recover it afterwards, and the
  whole-device node also spanned every partition the mount namespace did not.
  Would have scored `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N` — **3.3
  LOW**. The fix gates a block node's read on the entitlement the ext2 block
  reserve already asks for (a kernel thread or `TASK_FLAG_SYSTEM`).
- **A mount is a namespace change over the path a grant is keyed on.** The
  Phase 5 sweep's rule — "a privilege keyed on a path must seal every
  component of that path" — was satisfied by sealing the inodes, which a
  mount does not touch: a `Capability::Mount` holder could mount a fresh
  writable ramfs over `/bin`, plant `halt`, and receive `TASK_FLAG_POWER` from
  the path-keyed grant table. Not a ledger entry because no shipped program
  holds the capability, so there is no attacker-reachable trigger; the fix
  refuses a mount whose target equals or prefixes any path in
  `core::exec::grants`.
- **A default that is safe for one caller is a defect for another.** The file
  page set armed writeback for *any* mapping, so a `PROT_READ` shared mapping
  rewrote every page of an unmodified file on unmap — stamping its timestamps
  and, on a `VERITY=rw` image, un-attesting blocks nobody had written. The fix
  passes the mapping's write protection into `retain` and arms writeback only
  for a writable one.
- **Identity by address needs an address.** `same_filesystem` compares data
  addresses because a `&dyn FileSystem`'s vtable differs per coercing crate —
  but two of the filesystem statics were zero-sized, and Rust does not promise
  two distinct zero-sized statics distinct addresses. Had they aliased, the
  open-inode table, the cross-device rename check, the re-resolution check,
  the ramfs pool index and the page set's `holds` would all have confused ext2
  with devfs, whose inode numbers 1-6 collide directly. Each static now
  carries a byte, and a kernel test asserts the mounted ones are pairwise
  distinct.

The Phase 5 sweep found three defects, all fixed inside the same unreleased change
and therefore not ledger entries. They are recorded as method because each is
a class, not an instance:

- **A seal on the contents is not a seal on the name.** Every grant-path
  binary carried `EXT2_IMMUTABLE_FL`, so `/bin/halt` could not be overwritten
  — but `/bin` itself was an ordinary directory. `rename("/bin", "/x")`,
  `mkdir("/bin")`, plant a `halt`, and `exec::grants` confers `TASK_FLAG_POWER`
  on whatever was planted, because program identity is a path and the table is
  keyed on it (`core/src/exec/grants.rs`). Confidence 95 — evidence 40 (the
  grant table and the unsealed inode were both read directly), exploitability
  30 (three syscalls, any unprivileged process on the disk root; the RAM root
  had the same hole because ramfs never checked a parent's seal at all),
  reproducibility 25 (`grant_directories_are_sealed` in `spawn_privilege_test`
  was confirmed to fail on both roots with the fix reverted). Would have
  scored `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H` — **7.8 HIGH**. The
  fix seals `/bin` and `/sbin` on both roots and makes ramfs enforce a
  parent's seal on create, unlink and rename as ext2 already did. The general
  rule: a privilege keyed on a path must seal every component of that path,
  because the attack that replaces the leaf and the attack that replaces its
  parent reach the same grant.
- **A reserve on one table is not a reserve on the other.** ext2's
  `s_r_blocks_count` stops an unprivileged writer filling the *block* pool,
  but ext2 has no `s_r_inodes_count`: a process creating empty files exhausts
  the inode table with every block still free, and `/sbin/init` gets `ENOSPC`
  on its next create. Confidence 90 — the allocator was read, the fixture
  image has 4 096 inodes against 4 096 blocks, and
  `test_ext2_reserve_refuses_unprivileged_allocation` pins the inode half.
  Would have scored `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5
  MEDIUM**. The fix applies the block reserve's ratio to the inode table.
- **A blocking primitive re-entered by its own wake.** Not a security finding
  — no attacker chooses when a virtio completion lands — but recorded here
  because it was found by this sweep's method: the scheduler's deferred
  reschedule and trap-exit paths skipped a `Blocked` current task and not a
  `Ready` one, so a wake landing between the Blocked-CAS and the deschedule
  let `schedule()` dequeue the caller as its own successor and spin on its own
  `on_cpu` forever. A quarter of disk-root runs stalled on it. Reverting the
  fix reproduced 3 stalls in 10; with it, 0 in 21.

The previous sweep (2026-09-05, Phase 4) found two unprivileged denial-of-service defects
(`CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H`, 5.5 MEDIUM each) and one
cross-file disclosure race, all three introduced *and* fixed inside the same
unreleased change, so none is a ledger entry under the open-findings-only
policy below. They are recorded here as method rather than as findings, because
what produced them generalises: a new error classification that decides a
mount-wide policy must be checked against **every** producer of the errors it
names, in the direction of "can an unprivileged caller induce this on demand".
Both DoS defects were one overloaded error variant — `Ext2Error::InvalidBlock`
meaning both "the image is damaged" and "your offset is out of range" — and the
fix was to split the caller-argument case into `InvalidRange`, which
`is_corruption` deliberately excludes. `fs/src/tests.rs`'s
`test_ext2_corruption_latches_but_caller_errors_do_not` pins both directions
and was confirmed to fail against the vulnerable code.

This file is the living ledger of open security findings. Every entry recorded
since 2026-03-17 has been fixed and removed under the pre-alpha policy below;
what stays is the method and the tooling.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no backwards-compatibility
> or audit-trail obligations, so this ledger tracks **open findings only**. When a
> finding is resolved it is **removed** from this file, not retained as a `fixed`
> historical record. Internal IDs stay stable for findings that remain open, so
> gaps in the numbering are expected and IDs are never reused. The git history is
> the audit trail; `git log -- CVSS.md` recovers any entry that was here.

The highest ID issued so far is **SLOPOS-2026-0055**. The next finding is
`SLOPOS-2026-0056`.

## Method

A sweep is three phases, and the third is what makes the output trustworthy:

1. **Reference briefs.** Before reading SlopOS, read what mature kernels
   settled on for the subsystem in question — including designs they tried and
   abandoned, which is usually the more informative half.
2. **Per-subsystem auditors.** One pass per subsystem (syscall entry, memory
   management, filesystems, drivers, net, scheduler, authority model),
   producing candidate findings with exact paths and lines.
3. **Adversarial verification.** One verifier per candidate, instructed to
   default to **REFUTED** and to locate the guard that would disprove the
   claim. Two independent lenses for anything scoring High or above. A
   candidate that survives with a concrete attacker trigger becomes a finding;
   everything else is discarded or filed under `plans/` as an engineering
   defect.

### Required cadence

1. Run a security sweep after each major milestone and before any release or PR
   handoff.
2. Re-scan subsystems touched by recent commits — at minimum syscall paths,
   memory management, filesystems, and drivers.

### Triage workflow (strict order)

1. **List all findings first** in a raw triage section. Do not score as CVSS yet.
2. Assign each a **confidence score (0-100)**:
   - Evidence quality (0-40): direct code proof, exact path and line references
   - Exploitability clarity (0-30): realistic attacker path and impact
   - Reproducibility (0-30): deterministic repro or strong step-by-step plausibility
3. Only findings with **confidence >= 80** are **guaranteed issues**.
4. Only guaranteed issues get a CVSS vector and score.
5. Use `scripts/cvss_calc.py` so vectors are computed identically across agents.

A finding below 80 still belongs in this file if it has an attacker-reachable
trigger — it is recorded without a CVSS vector, and its confidence line says
what evidence would raise it.

**Findings with no attacker-reachable trigger are deliberately absent even at
high confidence**, because the non-negotiable rule below forbids presenting
speculative issues as scored vulnerabilities. Real engineering defects of that
kind are tracked in `plans/` instead.

### Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- Confidence-gated, evidence-backed findings only.
- **Verify the claim before fixing it.** A ledger entry can be wrong: entries
  have described bounds that were already total, and the real defect at the
  named site turned out to be different and worse. Read the code, not the
  entry.
- A fix lands with a test that **fails without it**. Confirm that by reverting
  the fix and re-running, never by assertion.

## Entry format

Every finding carries:

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps
- Remediation: the shape of the fix, not just "add a check"
```

### Repro requirements

1. Add a minimal repro for each guaranteed issue when technically feasible.
2. A repro may be a syscall sequence, a malformed input artifact, or concise
   PoC steps.
3. If no safe repro is possible, document why and give the nearest
   deterministic validation method.

## Scoring method

- CVSS version: 3.1 Base Score
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`

SlopOS has no credential model, so "unprivileged local attacker" means any
process that can execute code. Those are scored `AV:L/PR:L` by the usual
convention.

A panic in a `#![forbid(unsafe_code)]` crate is an **availability** impact,
never a memory-safety one, and is scored accordingly — several findings that
read as alarming are correctly `A:H` with `C:N/I:N`. Note that overflow checks
are on in the dev and tests kernels and off in release, so the same arithmetic
defect is a panic in one build and a silent wrong value in another; say which
when scoring.

### Reusable scorer

```bash
python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
```

### Prior-art lookup

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Analogs worth citing when they match a finding's shape:

| CVE | Vector | Score | Severity | Shape |
|---|---|---:|---|---|
| CVE-2016-5696 | CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:L/I:L/A:N | 4.8 | MEDIUM | Global counter side channel in a network stack |
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | ext* metadata parsing: a record individually legal but inconsistent with the walker's own arithmetic |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in a kernel path |

## Open findings at a glance

No open findings. The table returns when a sweep finds one; the format is
above, and the next ID is `SLOPOS-2026-0056`.
