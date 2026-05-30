# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17 (original); last reviewed 2026-05-25.
Method: repository-wide static review (`grep`, `ast-grep`, targeted source inspection), plus NVD CVE lookups via `curl` + `jq`.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no backwards-compatibility or audit-trail obligations, so this ledger tracks **open findings only**. When a finding is resolved it is **removed** from this file (not retained as a `fixed` historical record). Internal IDs stay stable for findings that remain open, so gaps in the numbering are expected.

## Scoring Method

- CVSS version: 3.1 Base Score
- Formula used: Base score derived from Impact + Exploitability subscores with scope-aware rounding up to one decimal
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`
- Reusable scorer script: `scripts/cvss_calc.py`

### Reusable scorer

```bash
python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
```

## Open SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0006
- Title: ext2 inode/group descriptor size trust can panic on out-of-bounds slicing
- Status: open
- Confidence: 70 (evidence 35, exploitability 15 — depends on whether SlopOS mounts untrusted images, reproducibility 20) — below the 80 threshold for a guaranteed CVSS-scored issue; the score below is an estimate carried from the original triage.
- Evidence: untrusted on-disk `inode_size` / derived offsets used in slice indexing without validating `within + size <= block_size`. `fs/src/ext2/ondisk.rs` (`Inode::parse`, `GroupDesc::parse`, `effective_inode_size` clamps `inode_size == 0` up to 128 but does not bound it from above against `block_size`) and the inode-table offset math in `fs/src/ext2/inode.rs` / `fs/src/ext2/mod.rs`.
- Impact: malformed-image-triggered out-of-bounds slice index. Because `fs/` is `#![forbid(unsafe_code)]`, this is a bounded-slice **panic** (DoS), never memory unsafety/UB.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:H`
- Base score: `6.1` (Medium)
- Remediation (proposed): validate `effective_inode_size() as u32 <= block_size` and bound each `within + size` against the containing block before slicing.

### SLOPOS-2026-0007
- Title: SlopRing `ring_enter` ↔ ring-fd `close` lock-order inversion (AB-BA) deadlocks the global ring registry
- Status: open
- Confidence: 82 (evidence 38: both opposite-order acquisitions traced to exact lines; exploitability 22: clear local attacker path with system-wide impact; reproducibility 22: deterministic AB-BA under the race, and the OSTD lock-graph cycle detector panics once both edges are learned) — at/above the 80 threshold, so CVSS-scored.
- Evidence:
  - Path A acquires the **global** ring registry lock then a **per-process** fileio-slot lock: `ring/src/enter.rs` `ring_enter` → `registry::with_ring(...)` holds `REGISTRY` (`ring/src/registry.rs:30,69-72`) across `submit`/`harvest_step` → `opcode::probe`/`reprobe` → `file_read_fd_nonblock`/`file_write_fd_nonblock` (`ring/src/opcode.rs`) → `file_read_fd_inner` → `lock_pid_slot` (`fs/src/fileio/fdops.rs:200`, the per-process `FileTableSlot.inner` SpinLock).
  - Path B acquires them in the opposite order: `file_close_fd` (`fs/src/fileio/fdops.rs:333`) → `with_pid_slot` holds the fileio slot lock → `release_open_file` (`fs/src/fileio/open_file_table.rs:63-82`) → `RingFileOps::release` (`ring/src/file_ops.rs:38`) → `registry::remove` → `REGISTRY`.
  - Both `REGISTRY` and every `FileTableSlot.inner` are `LOCK_LEVEL_REGISTRY`; the OSTD lock-graph cycle detector (`slopos-ostd/src/sync/lock_graph.rs`) reports a cycle once both edges are learned (panic), and under SMP it is a true AB-BA spin-deadlock.
- Impact: a local process holding a dup'd ring fd (intra-process dup is supported, SLOPRING § 6.3) with one thread in `ring_enter` and another closing the last ring fd can deadlock. Because `REGISTRY` is a single process-wide lock held forever in the deadlock, **every** other process's `ring_setup`/`ring_enter` then blocks → system-wide SlopRing denial of service (and two CPUs spin). Not triggered by `nc` (single-threaded, single ring) or the test suite (single-threaded ring use), so it is latent today.
- CVSS vector: `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:C/C:N/I:N/A:H`
- Base score: `5.6` (Medium)
- Repro (PoC): create a ring, `dup` its fd; thread A loops `ring_enter(fd_a, …)`; thread B loops `close(fd_b)` then re-creates — race until the two lock acquisitions interleave (or, with lock-tracking enabled, the cycle detector panics on the first interleave of both edges).
- Remediation (proposed): do not hold the registry lock across the fileio probe/release. Either (a) decouple per-ring serialization from the global registry lock and run `probe`/`reprobe` outside it (snapshot SQEs under the lock, probe lock-free, re-acquire to post — preserving the ownership-reserve/§ 11 ordering via a claimed CQ slot), or (b) make the generic fileio close path invoke `FileOps::release` *after* dropping the per-process slot + open-files locks so `registry::remove` never nests under them.

## Relevant NVD CVE Analogs (fetched)

Retrieved using NVD API pattern:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Selected analogs:

| CVE | Vector | Score | Severity | Why relevant |
|---|---|---:|---|---|
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | Filesystem metadata parsing / ext* class |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening analog |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in kernel path |

## Priority Remediation Plan

1. Break the SlopRing `ring_enter`/`close` lock-order inversion (SLOPOS-2026-0007) — run the fileio probe/release outside the global registry lock.
2. Guard all ext2 slice constructions with explicit bounds checks before indexing, including `effective_inode_size() <= block_size` (SLOPOS-2026-0006).
