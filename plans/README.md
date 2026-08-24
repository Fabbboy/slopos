# SlopOS Plans

This directory holds only live proposals and open work, written statelessly
against the current tree: a plan describes what to build and why, never what
was already built or how the plan evolved. When part of a plan lands, remove
that part and re-baseline the rest; when nothing remains, delete the plan.
Git history preserves the record. Stable public documentation lives in the
sibling docs repo.

Even live plans can carry stale paths after refactors — verify paths before
editing.

## Current Files

| Document | Scope |
|----------|-------|
| `KNOWN_ISSUES.md` | Working notes on open issues; verify before using as source of truth |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |
| `vcpu-steal-robustness.md` | Making the AP-pause, watchdog and harness bounds survive a host-descheduled vCPU |

The authority model has **landed and its plan is retired**. Authority is a flat
per-capability mask whose classification is total by compile-time construction:
`define_syscall!` takes a mandatory `cap(X)` clause and emits it into the dispatch table
through the handler, so a `const` histogram in `core/src/syscall/handlers.rs` asserts both
totality over all 177 slots and each capability's recorded entry-point count. Read the code
rather than a document: `slopos_ostd::authority` for the vocabulary and the witness,
`core/src/exec/grants.rs` for where authority enters, `slopos_ostd::seat` for the display
and input seats, and `verification/proofs/authority.rs` for the four machine-checked
obligations. `scripts/check_authority_reachability.sh` is what catches an unprivileged
syscall reaching a power primitive two calls away, which a slot-level gate cannot see.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
