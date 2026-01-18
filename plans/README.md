# SlopOS Development Plans

This directory contains architectural analysis and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS, with detailed analysis of current state and future directions |
| [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) | **NEW** Detailed implementation plan for the retained-mode widget toolkit |

---

## Current Focus: UI Toolkit

The kernel foundation is complete. All critical systems are implemented:
- VFS Layer with ext2, ramfs, devfs
- exec() syscall with ELF loading from filesystem
- libslop minimal C runtime (read/write/exit/malloc)
- CRT0, argv/envp passing, brk syscall
- Per-CPU page caches, VMA red-black tree
- ASLR, RwLock primitives
- Copy-on-Write, Demand Paging, fork() syscall
- SYSCALL/SYSRET fast path
- Priority-based scheduling
- TLB shootdown, FPU state save

### Remaining Work: UI Toolkit

No dependencies on kernel work. Can proceed immediately.

**See [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) for complete implementation details.**

| Task | Complexity | Status | Plan Reference |
|------|:----------:|:------:|----------------|
| Widget system API design | Low | | Phase 1 |
| Core widgets (Button, Label, Container) | Medium | | Phase 3 |
| Layout engine (Vertical, Horizontal, Grid) | Medium | | Phase 2 |
| Port shell to use toolkit | Medium | | Phase 5 |
| Theming system | Low | | Phase 4 |

#### Implementation Phases

1. **Phase 1**: Foundation - `Widget` trait, `WidgetRegistry`, event types, renderer integration
2. **Phase 2**: Basic Widgets - Button, Label, Theme system
3. **Phase 3**: Layout - Column, Row, Container with flex-based layout
4. **Phase 4**: Advanced Widgets - TextInput, Scrollable
5. **Phase 5**: Shell Migration - Port existing 1500-line shell to use toolkit

---

## Completed Milestones

All previous stages (1-4) and critical issues (P0-P2) have been resolved. See `ANALYSIS_SLOPOS_VS_LINUX_REDOX.md` for the current state comparison.

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
