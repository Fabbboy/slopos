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
| `process-object.md` | The `Process` object and the re-key of the address-space and descriptor tables off the recycled pid. Prerequisite for the two below |
| `resource-accounting.md` | Per-principal accounting: the `Account` tree, the linear `Charge` token, who pays per resource class |
| `authority-model.md` | What authority is: a flat per-process capability set, total by compile-time construction, with rights on objects |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

`process-object.md` → {`resource-accounting.md`,
`authority-model.md`} is a dependency order, not a suggestion: the latter two both need an
owner for their state, and the first two carry the fixes that would otherwise be hidden
rather than fixed by the frameworks above them.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
