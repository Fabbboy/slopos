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
| `resource-accounting.md` | **Mostly shipped.** Per-principal accounting: the `Account` tree, the linear `Charge` token, who pays per resource class. Seven of eight kinds are charged and enforced; `Pages` and reclaim are open |
| `authority-model.md` | What authority is: a flat per-process capability set, total by compile-time construction, with rights on objects |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

`resource-accounting.md` and `authority-model.md` both build on the `Process` object, which
has landed: `slopos_ostd::process` owns the identity, and the address-space and descriptor
tables are keyed on `Handle<Process>` rather than on a recycled pid. An account has an owner
to hang off and a credential has a principal to belong to.

Accounting has since shipped on top of that: `slopos_ostd::process::quota` is the arena and
the token, and `authority-model.md` can assume a live `Account` per process rather than
proposing one. What accounting did **not** land is reclaim — so the quota currently bounds
acquisition and not holding time, which is the first thing to read in that plan's
*What is left*.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
