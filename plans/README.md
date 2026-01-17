# SlopOS Development Plans

This directory contains architectural analysis, comparisons, and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS, with detailed analysis of memory management, scheduler, synchronization, and recommendations |
| [USERLAND_APP_SYSTEM_ANALYSIS.md](./USERLAND_APP_SYSTEM_ANALYSIS.md) | Analysis of implementing filesystem-loaded apps, libc/GNU toolchain support, and unified UI framework with dependency mapping to existing roadmap |
| [VFS_IMPLEMENTATION_PLAN.md](./VFS_IMPLEMENTATION_PLAN.md) | **Active** - Detailed implementation plan for the VFS layer including traits, mount table, ramfs, and devfs with phased approach |

---

## Roadmap

> **Current Focus**: Stage 3 (Performance & Security) or UI Toolkit
> 
> **Completed**: VFS Layer, exec() syscall, ramfs, devfs, libslop minimal, CRT0, brk syscall

### Stage 1: Foundation (Complete)

These items enable filesystem-loaded applications.

| Task | Type | Complexity | Depends On | Blocks | Status |
|------|------|:----------:|------------|--------|:------:|
| **VFS Layer** | Feature | High | - | exec(), ramfs, devfs | ✅ Complete |
| exec() syscall | Feature | Medium | VFS | libslop, /bin apps | ✅ Complete |
| ramfs (/tmp, /dev) | Feature | Low | VFS | - | ✅ Complete |
| devfs | Feature | Low | VFS | - | ✅ Complete |

### Stage 2: Userland Runtime (Complete)

Once VFS + exec() are done, build the minimal C runtime for external apps.

| Task | Type | Complexity | Depends On | Blocks | Status |
|------|------|:----------:|------------|--------|:------:|
| libslop minimal (read/write/exit/malloc) | Feature | High | exec() | external apps | ✅ Complete |
| CRT0 (_start entry point) | Feature | Low | exec() | libslop | ✅ Complete |
| argv/envp passing | Feature | Low | exec() | libslop | ✅ Complete |
| brk syscall (heap management) | Feature | Medium | exec() | malloc | ✅ Complete |
| Cross-compiler target (x86_64-slopos) | Tooling | Low | libslop | - | ⚠️ Exists |

### Stage 3: Performance & Security

Can be worked on **in parallel** with Stage 1-2. No hard dependencies.

| Task | Type | Complexity | Depends On | Status |
|------|------|:----------:|------------|:------:|
| Per-CPU page caches | Performance | Medium | - | |
| O(n) VMA lookup → tree/RB-tree | Performance | Medium | - | |
| ASLR | Security | Medium | - | |
| RwLock primitive | Feature | Low | - | |

### Stage 4: Advanced Memory

Required for efficient fork() and full POSIX compatibility (Tier 3 userland).

| Task | Type | Complexity | Depends On | Status |
|------|------|:----------:|------------|:------:|
| Copy-on-Write (CoW) | Feature | High | - | |
| Demand Paging | Feature | Medium | - | |
| fork() syscall | Feature | Medium | CoW | |

### Parallel Track: UI Toolkit

No dependencies on VFS/exec. Can start immediately.

| Task | Complexity | Status |
|------|:----------:|:------:|
| Widget system API design | Low | |
| Core widgets (Button, Label, Container) | Medium | |
| Layout engine (Vertical, Horizontal, Grid) | Medium | |
| Port shell to use toolkit | Medium | |
| Theming system | Low | |

---

## Dependency Graph

```
 STAGE 1 (DONE)          STAGE 2 (DONE)          STAGE 3-4
┌──────────────┐        ┌──────────────┐        ┌──────────────┐
│  VFS Layer ✅│───────►│  libslop ✅  │        │  Per-CPU     │
└──────────────┘        └──────┬───────┘        │  page cache  │
       │                       │                └──────────────┘
       ├──► ramfs ✅           │                ┌──────────────┐
       │                       │                │  VMA tree    │
       ├──► devfs ✅           ▼                └──────────────┘
       │                ┌──────────────┐        ┌──────────────┐
       └──► exec() ✅   │ Cross-comp ⚠️│        │    ASLR      │
                        └──────┬───────┘        └──────────────┘
                               │                ┌──────────────┐
                               ▼                │   RwLock     │
                        ┌──────────────┐        └──────────────┘
                        │  /bin apps   │        ┌──────────────┐
                        └──────────────┘        │    CoW       │───► fork()
                                                └──────────────┘

 PARALLEL TRACK
┌──────────────────────────────────────┐
│            UI Toolkit                │  (no dependencies, start anytime)
└──────────────────────────────────────┘
```

---

## Completed Issues

### P0 - Critical (All Fixed)

- [x] **No FPU state save** - SSE/AVX registers corrupted across task switches *(Fixed: added FXSAVE/FXRSTOR to context switch)*
- [x] **No TLB shootdown** - Will cause memory corruption on SMP *(Fixed: IPI-based TLB shootdown in mm/src/tlb.rs with per-CPU state, INVPCID detection, callback-based IPI sender)*
- [x] **Syscall table overflow** - Potential code execution if sysno >= 128 *(Fixed: syscall_lookup() bounds-checks against SYSCALL_TABLE.len())*
- [x] **ELF loader validation** - Insufficient input validation *(Fixed: comprehensive ElfValidator with bounds checking, overflow prevention, segment overlap detection, and address space validation)*

### P1 - Performance (Partial)

- [x] **`int 0x80` syscalls** - 3x slower than `syscall` instruction *(Fixed: SYSCALL/SYSRET fast path with SWAPGS, per-CPU kernel stack, canonical address validation)*
- [x] **Priority field unused** - Scheduler ignores task priorities *(Fixed: priority-based ready queues array with 4 levels, select_next_task scans HIGH→IDLE)*

---

## Recommended Reading Order

1. Start with the **Executive Summary** in the analysis document
2. Review **Section 8: Critical Issues Summary** for immediate priorities
3. Check **Section 9: Recommendations Roadmap** for phased implementation plan
4. For userland app system, read USERLAND_APP_SYSTEM_ANALYSIS.md sections 4-5
5. Deep-dive into specific subsystems as needed

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
