# SlopOS Development Plans

This directory contains architectural analysis and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS, with detailed analysis of current state and future directions |

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

| Task | Complexity | Status |
|------|:----------:|:------:|
| Widget system API design | Low | |
| Core widgets (Button, Label, Container) | Medium | |
| Layout engine (Vertical, Horizontal, Grid) | Medium | |
| Port shell to use toolkit | Medium | |
| Theming system | Low | |

---

## Completed Milestones

All previous stages (1-4) and critical issues (P0-P2) have been resolved. See `ANALYSIS_SLOPOS_VS_LINUX_REDOX.md` for the current state comparison.

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
