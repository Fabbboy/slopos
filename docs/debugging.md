# Kernel debugging with QEMU + GDB

SlopOS has two complementary GDB workflows for chasing kernel bugs under QEMU.
Both load **kernel + userland symbols** and a **page-table walker**, and break
at the fault paths. Pick based on whether the bug reproduces under a single CPU.

| Workflow | Recipe / script | Use when |
|---|---|---|
| **Live forward** (KVM, smp=4, real timing) | `scripts/qemu_dbg.sh` + `scripts/gdb/inspect_fault.gdb` | The bug only reproduces under the production timing/SMP config. No reverse-continue, but you can break at a fault and inspect state forward. |
| **Record/replay** (TCG, smp=1, deterministic) | `just rr-record` / `just rr-replay` / `just rr-gdb` | You need `reverse-continue` / reverse watchpoints to find *who* wrote a corrupted byte. Cannot capture SMP-only races or bugs whose timing changes under `icount`. |

## Shared GDB tooling (`scripts/gdb/`)

- `slopos_mmu.py` — defines `v2p <cr3> <va>` (walk PML4→PT, print each level + flags, resolve VA→PA) and `wpva <cr3> <va>` (resolve, then set a **physical** write watchpoint that fires regardless of which CR3/alias performs the write).
- `slopos.gdb` — interactive driver (replay): dual symbols, `udinfo`, fault breakpoints.
- `inspect_fault.gdb` — batch forward inspection: break at the user fault, dump the frame, the **actual bytes at the faulting RIP** (vs. what the ELF says), and page-table walks for RIP and RSP.
- `find_corruptor.gdb` — batch replay: break at the fault, then `wpva` + `reverse-continue` to the writing instruction (set `$watch_va`).

GDB gotchas (the kernel is Rust):
- GDB defaults to **Rust** expression mode — use `(*frame).rip`, not `frame->rip`.
- Use **`hbreak`** for kernel symbols: software breakpoints can't be written to kernel VAs that aren't mapped yet at reset.
- `pub(crate)` fns need file:line targets (`hbreak exception.rs:43`) to avoid name ambiguity.

## Live forward debugging (KVM)

```bash
# 1. Build a test ISO (or use an existing one), then launch QEMU halted with the gdbstub:
QEMU_SMP=4 scripts/qemu_dbg.sh builddir/slop-tests.iso fs/assets/ext2-tests.img &   # background
# 2. Attach the inspector (breaks at the first user fault, dumps everything):
gdb -q -batch -x scripts/gdb/inspect_fault.gdb
```

`inspect_fault.gdb` answers the canonical "#UD on a valid instruction?" question
directly: it disassembles the *actual guest memory* at the faulting RIP. If the
bytes aren't the expected instruction, the page is corrupted / mis-mapped — then
`v2p` shows which physical frame backs it.

Swap the userland binary under inspection with `SLOPOS_USER_ELF=builddir/<bin>.elf`.

## Record/replay reverse debugging (TCG)

```bash
just rr-record       # records a deterministic failing run to builddir/replay.bin
just rr-replay       # replays under interactive GDB (gdbstub halted on :1234)
# or, batch "find the writer":
just rr-gdb WATCH=0x4090fe
```

`scripts/qemu_rr.sh` enforces the icount/record-replay constraints (TCG, `-smp 1`,
`-cpu max`, no iothread, `blkreplay`-wrapped disks, `filter-replay` on the NIC,
read-only UEFI vars). Some kernel tests assert wall-clock timer calibration and
fail under instruction-counted time; `rr_skip` in the `justfile` lists those so
recording reaches the workload.

**Reverse playbook:** record → replay to the fault → `v2p $cr3 <corrupted-va>` →
`wpva $cr3 <corrupted-va>` → `reverse-continue`. The stop is the instruction that
last wrote those bytes; compare its `$cs`/`$cr3` to classify the writer (kernel
vs user, same vs foreign address space).

**Caveat:** record/replay needs `icount` + single CPU. If a bug's manifestation
depends on KVM/SMP timing (or `icount` changes scheduling enough to deadlock the
workload), use the live forward workflow instead.
