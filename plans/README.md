# SlopOS Development Plans

This directory contains architectural analysis, comparisons, and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS, with detailed analysis of memory management, scheduler, synchronization, and recommendations |

## Quick Reference: Priority Issues

### P0 - Critical (Fix Immediately)

- [x] **No FPU state save** - SSE/AVX registers corrupted across task switches *(Fixed: added FXSAVE/FXRSTOR to context switch)*
- [x] **No TLB shootdown** - Will cause memory corruption on SMP *(Fixed: IPI-based TLB shootdown in mm/src/tlb.rs with per-CPU state, INVPCID detection, callback-based IPI sender)*
- [x] **Syscall table overflow** - Potential code execution if sysno >= 128 *(Fixed: syscall_lookup() bounds-checks against SYSCALL_TABLE.len())*
- [x] **ELF loader validation** - Insufficient input validation *(Fixed: comprehensive ElfValidator with bounds checking, overflow prevention, segment overlap detection, and address space validation)*

### P1 - Performance

- [ ] **No per-CPU page caches** - Global lock contention on every allocation
- [ ] **`int 0x80` syscalls** - 3x slower than `syscall` instruction *(SYSCALL/SYSRET attempted but caused freeze after roulette - reverted, needs investigation)*
- [ ] **O(n) VMA lookup** - Linked list doesn't scale
- [ ] **Priority field unused** - Scheduler ignores task priorities

### P2 - Missing Features

- [ ] ASLR (Address Space Layout Randomization)
- [ ] Copy-on-Write / Demand Paging
- [ ] VFS Layer
- [ ] RwLock primitive

## Recommended Reading Order

1. Start with the **Executive Summary** in the analysis document
2. Review **Section 8: Critical Issues Summary** for immediate priorities
3. Check **Section 9: Recommendations Roadmap** for phased implementation plan
4. Deep-dive into specific subsystems as needed

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
