# SlopOS

SlopOS is a gloriously sloppy x86-64 kernel where every boot spins the Wheel of Fate. Subsystems live under familiar directories (`boot/`, `mm/`, `drivers/`, `sched/`, `video/`) while lore and contributor guidance stay in `AGENTS.md`. Read that file first—it sets the tone and workflow expectations.

## Build Workflow

All day-to-day work goes through the Makefile. Typical targets:

- `make setup` (once per checkout) to configure Meson/Ninja
- `make build` to compile the kernel
- `make boot`, `make boot-log`, `make test` when you need QEMU runs

Artifacts land in `builddir/`, and `test_output.log` captures non-interactive boots.

## MCP Quick Start


That’s all you need to get started; everything else (style, lore, subsystem breakdowns, and MCP details) lives in `AGENTS.md`. Go spin the wheel.
