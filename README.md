# SlopOS

SlopOS is a gloriously sloppy x86-64 kernel where every boot spins the Wheel of Fate. Subsystems live under familiar directories (`boot/`, `mm/`, `drivers/`, `sched/`, `video/`) while lore and contributor guidance stay in `AGENTS.md`. Read that file first—it sets the tone and workflow expectations. The kernel is now a Rust-first build on nightly, keeping the same module split while we retire the legacy C sources.

## Key Features

- **Full Privilege Separation**: Ring 0 (kernel) and Ring 3 (user mode) with proper GDT, TSS, and page table isolation
- **Cooperative Scheduler**: Task switching with preemption support
- **Syscall Interface**: `int 0x80` gateway for user→kernel transitions
- **Memory Management**: Buddy allocator, paging, process VM spaces
- **User Mode Tasks**: Shell and roulette run in Ring 3 with syscall-based kernel services
- **The Wheel of Fate**: Kernel roulette system with W/L currency for the gambling-addicted wizards
- **Filesystem Migration (WIP)**: ext2 backend is the sole filesystem (`ext2_init_with_image` is required)

For detailed documentation on privilege separation and segment switching, see [`docs/PRIVILEGE_SEPARATION.md`](docs/PRIVILEGE_SEPARATION.md).

## Build Workflow (Rust)

- `make setup` installs the pinned nightly from `rust-toolchain.toml` (via rustup) and primes `builddir/`.
- `make build` compiles the Rust kernel with `cargo` using the custom target JSON at `targets/x86_64-slos.json`.
- `make iso`, `make boot`, `make boot-log`, `make test` keep the same UX as before; the Makefile still handles Limine/OVMF fetching and ISO assembly.

Artifacts land in `builddir/` (kernel at `builddir/kernel.elf`, cargo intermediates in `builddir/target/`), and `test_output.log` captures non-interactive boots.

### Video requirement

SlopOS boots only with a Limine-provided framebuffer; video output is mandatory and the kernel will panic if no framebuffer is available. Run under QEMU/OVMF with GOP enabled (e.g., `-machine q35`) so Limine can hand off a valid framebuffer.

That's all you need to get started; everything else (style, lore, and subsystem breakdowns) lives in `AGENTS.md`. Go spin the wheel.
