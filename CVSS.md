# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17 (original); last reviewed 2026-07-27.
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
- Confidence: 70 (evidence 35, exploitability 15 — depends on whether SlopOS mounts untrusted images, reproducibility 20) — below the 80 threshold for a guaranteed CVSS-scored issue.
- Evidence: untrusted on-disk `inode_size` / derived offsets used in slice indexing without validating `within + size <= block_size`. `fs/src/ext2/ondisk.rs` (`Inode::parse`, `GroupDesc::parse`, `effective_inode_size` clamps `inode_size == 0` up to 128 but does not bound it from above against `block_size`) and the inode-table offset math in `fs/src/ext2/inode.rs` / `fs/src/ext2/mod.rs`.
- Impact: malformed-image-triggered out-of-bounds slice index. Because `fs/` is `#![forbid(unsafe_code)]`, this is a bounded-slice **panic** (DoS), never memory unsafety/UB.
- CVSS vector/score: not assigned because confidence is below 80.
- Remediation (proposed): validate `effective_inode_size() as u32 <= block_size` and bound each `within + size` against the containing block before slicing.

### SLOPOS-2026-0007
- Title: RCU quiescent states are reported from inside read-side critical sections, so a grace period can complete while a reader holds a reference
- Status: open
- Confidence: 86 (evidence 38 — direct code proof at exact paths on both the reader and reporter sides; exploitability 24 — remote packet-driven path through the TCP connection table plus several local paths, but the window is a race; reproducibility 24 — the invariant violation is deterministically demonstrable, an end-to-end use-after-free is timing-dependent) — at or above the 80 threshold, so scored.
- Provenance: the root defect **predates** the task-ownership migration audited in this sweep. `scheduler_timer_tick`'s unconditional quiescent state came from `65154a33`, not from `c308a219..HEAD`. The sweep surfaced it because `84dd87cf` rewrote the `rcu_call_typed` deferral and `c308a219` added `RcuArcSlot`, whose safety argument rests explicitly on the property that is broken.
- Evidence:
  - `slopos-ostd/src/sync/rcu.rs:64-68` — `rcu_read_lock()` returns a guard holding only a `PreemptGuard`. Preemption is disabled; **interrupts are not**.
  - `slopos-ostd/src/cpu/preempt.rs:347-367` — `PreemptGuard::new()` is a single `preempt_count_inc()`. Its own doc states the guard is constructed "at the preemptible baseline (count == 0, IRQs on)".
  - `slopos-ostd/src/sync/rcu.rs:74-79` — `rcu_note_qs()` is an unconditional `RCU_QS_CTR[cpu].fetch_add(1, Release)`. It does not consult the preempt count.
  - `sched/src/scheduler.rs:2378-2382` — the timer ISR calls `rcu_note_qs()` unconditionally. The comment above it inverts its own premise: "the timer ISR firing proves this CPU is not inside an RCU read-side critical section (those disable preemption but not interrupts)". Because sections disable preemption but not interrupts, the tick *can* fire inside one.
  - `boot/src/idt.rs:513-517` — the `RCU_QS_IPI_VECTOR` handler also calls `rcu_note_qs()` unconditionally, so `synchronize_rcu`'s own stall-breaker IPI (`slopos-ostd/src/sync/rcu.rs:233-238`) can force a false quiescent state on a CPU that is mid-read.
  - `slopos-ostd/src/sync/rcu.rs:203-257` — `synchronize_rcu()` snapshots each CPU's counter and waits only for it to advance. One tick inside a reader's section satisfies that wait.
  - Affected readers: `slopos-ostd/src/sync/epoch.rs:65-78` (`Epoch::enter` is built on `rcu_read_lock`, so it inherits the defect and adds no protection of its own), `net/src/tcp/table.rs:296-300` (`TCP_SHARDS_INDEX`, `TCP_LISTENERS_INDEX`, read per packet), `net/src/xdp/mod.rs:69` (filter chain), `font/src/atlas.rs:560` (glyph atlas), and `slopos-ostd/src/sync/rcu.rs` `RcuArcSlot::load` (no production consumer yet).
- Impact: a reader inside a read-side critical section can have the object it is reading freed underneath it. For `RcuCellGuard` the exposure is the whole guard lifetime, since the guard derefs to `&T`; for `RcuArcSlot::load` it is the window between the `Acquire` load and the refcount increment. Kernel use-after-free — read and, via a resurrected refcount, write.
- CVSS vector/score: `CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:H/I:H/A:H` = **8.1 HIGH** (`scripts/cvss_calc.py`). `AV:N` because both sides of the race are driven by remote packets on the TCP index path (per-packet `find` against connection insert/remove); `AC:H` because it is a timing race. The local-only readers (glyph atlas) score `CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:H/I:H/A:H` = 7.0 HIGH.
- Repro: an end-to-end use-after-free is timing-dependent and no deterministic trigger is available, because it needs a tick to land in a window of a few instructions on one CPU while another CPU sits between its `synchronize_rcu` snapshot and its callback invocation. Nearest deterministic validation, which demonstrates the broken invariant directly rather than its consequence:
  1. On CPU A, take `let g = rcu_read_lock();` and hold it.
  2. Call `rcu_note_qs()` while `g` is alive — this is exactly what `scheduler_timer_tick` does on a tick, and what the QS IPI handler does.
  3. From CPU B, call `synchronize_rcu()`.
  4. Observe that it returns while `g` is still alive. A correct implementation cannot return until `g` is dropped.

  The static half needs no runtime at all: `rcu_note_qs` contains no preempt-count check, so no caller of it can distinguish in-section from out-of-section.
- Remediation (proposed): make the quiescent state conditional on not being inside a read-side section — report only when `preempt_count() == 0` at the tick and IPI sites, and have `synchronize_rcu` treat a CPU whose count is non-zero as not yet quiescent (deferring its QS to the `PreemptGuard` drop that returns the count to zero). Correcting the inverted comment at `sched/src/scheduler.rs:2378` is part of the fix, not a substitute for it.

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

1. Make RCU quiescent-state reporting preempt-count aware so a grace period cannot complete while a read-side critical section is open (SLOPOS-2026-0007). This is the higher priority of the two: it is scored HIGH, it is remotely reachable, and every current and future `RcuCell` / `Epoch` / `RcuArcSlot` reader depends on the guarantee it breaks.
2. Guard all ext2 slice constructions with explicit bounds checks before indexing, including `effective_inode_size() <= block_size` (SLOPOS-2026-0006).
