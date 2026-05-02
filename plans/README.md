# SlopOS Development Plans

This directory contains architectural analysis and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS |
| [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) | Detailed implementation plan for the retained-mode widget toolkit |
| [KNOWN_ISSUES.md](./KNOWN_ISSUES.md) | Open performance issues and notes for future development |
| [WILLBLOCK_SCHEDULER_STATUS.md](./WILLBLOCK_SCHEDULER_STATUS.md) | WillBlock task state refactor status — replaced pending_wakeup, CompletionEvent scheduler path deferred |
| [LEGACY_MODERNIZATION_PLAN.md](./LEGACY_MODERNIZATION_PLAN.md) | 8-phase roadmap to replace all legacy hardware (PIT, FXSAVE, port-I/O PCI, naive spinlocks) with modern equivalents (HPET, XSAVE, ECAM, ticket locks, MSI, TCP, PCID) — 109 tasks |
| [SHELL_EVOLUTION_PLAN.md](./SHELL_EVOLUTION_PLAN.md) | 7-phase plan to evolve the shell from command dispatcher to real POSIX-inspired shell (163 tasks) |
| [NETWORKING_EVOLUTION_PLAN.md](./NETWORKING_EVOLUTION_PLAN.md) | 9-phase roadmap to evolve SlopOS networking from prototype to BSD-socket-compatible TCP/IP stack — ~125 tasks |
| [NETCAT_PLAN.md](./NETCAT_PLAN.md) | nc (netcat) implementation plan |
| [PHASE8_UNSAFE_INVARIANT_REGISTRY.md](./PHASE8_UNSAFE_INVARIANT_REGISTRY.md) | Unsafe invariant registry for legacy modernization Phase 8 |
| [FRAMEKERNEL_PLAN.md](./FRAMEKERNEL_PLAN.md) | 4-phase rip-and-replace plan to redesign SlopOS as an async-first framekernel with a Verus-verified OSTD critical path. Inspired by Asterinas (USENIX ATC '25). |


---

## Completed: SMP Safety Infrastructure

The Rust-native SMP safety infrastructure is now complete:

| Component | Status |
|-----------|--------|
| `TaskStatus` enum | Implemented in `abi/src/task.rs` |
| `BlockReason` enum | Implemented in `abi/src/task.rs` |
| Type-safe state transitions | `mark_ready()`, `mark_running()`, `block()`, `terminate()` |
| `TaskHandle` safe wrapper | Implemented in `core/src/scheduler/task_lock.rs` |
| `IrqRwLock` primitives | Implemented in `lib/src/spinlock.rs` |
| `SwitchContext` with `offset_of!` | Compile-time safe struct offsets |

---

## Completed: Unified PCR Infrastructure

The Unified Processor Control Region (PCR) following Redox OS patterns is complete:

| Component | Status |
|-----------|--------|
| `ProcessorControlRegion` struct | Implemented in `lib/src/pcr.rs` |
| Per-CPU GDT/TSS embedding | Embedded in PCR |
| Fast GS-based CPU access | `gs:[24]` for instant CPU ID (~1-3 cycles) |
| AP user-mode execution | Fixed - tasks run on any CPU |
| SYSCALL/context switch assembly | Updated for PCR offsets |

---

## Completed: Kernel Foundation

The kernel foundation is complete. All critical systems are implemented:
- VFS Layer with ext2, ramfs, devfs
- exec() syscall with ELF loading from filesystem
- libc minimal C runtime (read/write/exit/malloc)
- CRT0, argv/envp passing, brk syscall
- Per-CPU page caches, VMA red-black tree
- ASLR, RwLock primitives
- Copy-on-Write, Demand Paging, fork() syscall
- SYSCALL/SYSRET fast path
- Priority-based scheduling
- TLB shootdown, FPU state save

---

## Completed: Scheduler Unification

The scheduler unification is complete. All CPUs now use unified code paths:

| What | Status |
|------|:------:|
| Timer tick drains inbox for ALL CPUs | ✅ Done |
| Lock-free cross-CPU via `push_remote_wake()` | ✅ Done |
| BSP uses unified `scheduler_loop()` | ✅ Done |
| Unified `enter_scheduler(cpu_id)` entry point | ✅ Done |
| Lock-free atomics for hot-path checks | ✅ Done |
| Legacy wrappers removed | ✅ Done |

**Current State**: Complete. All 364 tests pass.

**Optional Cleanup**: [SCHEDULER_FULL_SYMMETRY.md](./SCHEDULER_FULL_SYMMETRY.md) documents remaining asymmetries that could be eliminated (low priority).

---

## Next: UI Toolkit

**See [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) for complete implementation details.**

| Task | Complexity | Status | Plan Reference |
|------|:----------:|:------:|----------------|
| Widget system API design | Low | | Phase 1 |
| Core widgets (Button, Label, Container) | Medium | | Phase 3 |
| Layout engine (Vertical, Horizontal, Grid) | Medium | | Phase 2 |
| Port shell to use toolkit | Medium | | Phase 5 |
| Theming system | Low | | Phase 4 |

### Implementation Phases

1. **Phase 1**: Foundation - `Widget` trait, `WidgetRegistry`, event types, renderer integration
2. **Phase 2**: Basic Widgets - Button, Label, Theme system
3. **Phase 3**: Layout - Column, Row, Container with flex-based layout
4. **Phase 4**: Advanced Widgets - TextInput, Scrollable
5. **Phase 5**: Shell Migration - Port existing 1500-line shell to use toolkit

---

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
