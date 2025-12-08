# SlopOS

SlopOS is a gloriously sloppy x86-64 kernel where every boot spins the Wheel of Fate. Subsystems live under familiar directories (`boot/`, `mm/`, `drivers/`, `sched/`, `video/`) while lore and contributor guidance stay in `AGENTS.md`. Read that file first—it sets the tone and workflow expectations.

## Key Features

- **Full Privilege Separation**: Ring 0 (kernel) and Ring 3 (user mode) with proper GDT, TSS, and page table isolation
- **Cooperative Scheduler**: Task switching with preemption support
- **Syscall Interface**: `int 0x80` gateway for user→kernel transitions
- **Memory Management**: Buddy allocator, paging, process VM spaces
- **User Mode Tasks**: Shell and roulette run in Ring 3 with syscall-based kernel services
- **The Wheel of Fate**: Kernel roulette system with W/L currency for the gambling-addicted wizards

For detailed documentation on privilege separation and segment switching, see [`docs/PRIVILEGE_SEPARATION.md`](docs/PRIVILEGE_SEPARATION.md).

## Build Workflow

All day-to-day work goes through the Makefile. Typical targets:

- `make setup` (once per checkout) to configure Meson/Ninja
- `make build` to compile the kernel
- `make boot`, `make boot-log`, `make test` when you need QEMU runs

Artifacts land in `builddir/`, and `test_output.log` captures non-interactive boots.

### Video requirement

SlopOS boots only with a Limine-provided framebuffer; video output is mandatory and the kernel will panic if no framebuffer is available. Run under QEMU/OVMF with GOP enabled (e.g., `-machine q35`) so Limine can hand off a valid framebuffer.

## MCP Quick Start


That’s all you need to get started; everything else (style, lore, subsystem breakdowns, and MCP details) lives in `AGENTS.md`. Go spin the wheel.
