---
name: terminal-resize-scrollback-realloc-oom
description: Terminal dies with a fatal user page fault when resizing the window — scrollback realloc churn OOMs the demand-fault path
metadata:
  type: project
---

Terminal task killed by a fatal user page fault during an interactive window
resize ("EXCEPTION: Vector 14 ... Error code 0x6 (not present)(Write)(User)",
fault inside a `vmovups` AVX blank-fill). RESOLVED 2026-06-22.

**Root cause (userland design):** `TerminalGrid::resize` in `terminal-core/src/grid.rs`
did `self.scrollback = ScrollbackBuf::new(cols)` on every width change, which
allocates AND blank-fills a fresh `vec![Cell::blank(); SCROLLBACK_LINES(1000) * cols]`
ring — up to 1000×240×12 = ~2.75 MiB — while the old ring is still live (RHS
evaluated before the assignment drops the old one). An interactive drag fires a
width change for every cell-width step, so this multi-MB alloc+demand-fill churns
dozens of times/sec, transiently doubling. The kernel demand-faults the lazy anon
mmap (`mm/demand.rs::handle_demand_fault` → `alloc_kernel_page`); when a page alloc
returns null (physical OOM), `try_resolve_user_fault` returns false and
`boot/exception.rs::exception_page_fault` UNCONDITIONALLY terminates the user task
(no extra log → silent `MmError::NoMemory`).

**Diagnosis trick:** disassemble the faulting RIP in `builddir/terminal.elf`. The
`imul $0x3e8,%rbx` (×1000 = SCROLLBACK_LINES) fill-loop bound is the tell it's the
scrollback ring (cells/alt would be ×rows). `RBX`=cols, `R15`=cols×1000 cells.

**Fix (all three large grid buffers, grow-once + reuse-in-place):**
- `reserve_to_max(buf, max)` helper: `buf.reserve_exact(max - len)` only when
  `capacity() < max`. Reserves capacity straight to the ceiling ONCE; the anon-mmap
  backing is lazy so only the `len`-sized live region faults in (typical terminals
  stay lean; no per-step realloc on an outward drag).
- `ScrollbackBuf`: `new(cols)` sizes `SCROLLBACK_LINES*cols`; `reset_for_width(cols)`
  grows `len` to the new high-water (reserving cap to `SCROLLBACK_LINES*MAX_COLS`
  once), keeps the larger `len` on shrink, resets head/count/view_offset/cols.
  History is dropped on width change (fixed-width rows can't reflow); safe because
  `count=0` gates every reader and `push_row` overwrites a row before `get_row`.
- `CellGrid::allocate` reserves cap to `MAX_ROWS*MAX_COLS`; `len` still tracks the
  logical grid so get/set/row_copy/clear_all are unchanged.
- `TerminalGrid` holds a pooled `resize_scratch: CellGrid`; `resize` does
  `scratch.allocate(r,c); scratch.copy_from(&cells, copy_r, copy_c); mem::swap(cells, scratch)`
  (same for alt_cells) instead of building fresh grids. Old backings park in the
  pool and are reused; after warmup all three backings sit at MAX capacity and
  NEVER reallocate. Closes the residual cells/alt churn (same crash class was
  reachable on tall+wide drags: >~45 rows at 240 cols pushes a CellGrid over the
  128 KiB dlmalloc mmap_threshold → direct-mmap churn per step).
- Tests: `width_resize_grows_scrollback_at_most_once`,
  `resize_drag_does_not_reallocate_cell_grids` (assert capacities pinned at the
  ceiling across a sweep, content preserved). 45 host + 2593 QEMU tests pass.

General lesson: never reallocate+demand-fill a large buffer on a high-frequency UI
event (resize/drag). Reserve capacity to the bound once (lazy backing keeps it
cheap), keep a pooled scratch to reuse allocations, and re-stride/reflow in place.
An adversarial subagent design review caught that fixing only the scrollback left
the same class reachable via cells/alt — fix the whole class, not the loudest case.
Related: [[mmap-only-malloc-direction]], [[brk-contract-byte-granular]],
[[world-class-no-tinkering]].
