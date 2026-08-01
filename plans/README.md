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
| `privilege-model.md` | Spike: what authority should actually be, given no uid and no credential object |
| `rt-sigreturn-xrstor.md` | Validate the signal-return XSAVE image; make a ring-0 #GP survivable |
| `resource-accounting.md` | Spike: per-process limits. Inventory + the reclaim fixes that need no framework |
| `deferred-work.md` | Per-CPU work list so RCU reclaim stops depending on CPU 0 being idle |
| `lockdep-effectiveness.md` | The lock-order validator exhausts its class table during mm init and turns itself off |
| `lockup-detector.md` | The watchdog's two time graces are defects, not tuning; index the unwinder, then measure progress instead of elapsed time |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
