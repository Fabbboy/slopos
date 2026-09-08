# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

Nothing in the tree states this goal today. This document is the anchor; the
phases below are ordered by what physically blocks the next measurement, not by
appetite.

**Scale, from the host, measured:** the pinned toolchain sysroot is 1.1 GB;
`librustc_driver.so` is a single 161 MB shared object loaded through `PT_INTERP`;
`builddir/target` holds 52 GB across 219,895 files. Against that, SlopOS today
caps an executable at 1 MiB (`mm/src/slab/mod.rs:61` bounds the `KVec` at
`core/src/exec/mod.rs:463`), ships a 32 MiB root image with ~2 k inodes, and
refuses `PT_INTERP` outright (`mm/src/elf.rs:591`). The gap is three orders of
magnitude in four independent dimensions, and every constant that produces it
was chosen correctly for an appliance.

**The theme of this plan:** SlopOS's limits are not architectural mistakes, they
are appliance-sized constants and appliance-sized policies. A workbench needs
those quantities derived from the medium (image size, RAM, file size) instead of
frozen at values that fit a test fixture. The work is mostly *widening under
proof*, not redesign — with four genuine exceptions (demand-paged exec, a
sleepable page-fault path, dynamic linking, and the compiler bootstrap itself).

## Architectural constraints (do not violate)

- **Unsafe surface.** Only `slopos-ostd` may use `unsafe`; every other kernel
  crate stays `#![forbid(unsafe_code)]` and `check_unsafe_expansion.sh` sees
  through macros. Nothing in this plan earns an exemption.
- **Allocation discipline.** `KBox`/`KVec`/`KArc`/`KBTreeMap` only. Every
  toolchain-sized buffer this plan touches must become a chunked or page-list
  design rather than a bigger single allocation: `MAX_ALLOC_SIZE` is 1 MiB
  (`mm/src/slab/mod.rs:61`) and raising it is not the fix.
- **Stack frames ≤ 2 KiB** against a 4 KiB guard page. This is why
  `MAX_PATH_LEN` is 256 (`fs/src/lib.rs:4`); widening paths means moving
  `CanonPath`/`MountPoint.path` off the stack, not raising the gate.
- **Task ownership I1–I8** and **no `async fn` in a kernel crate**. A sleepable
  fault path (Phase 1) must be a blocking kernel task, not an executor.
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source, ever
  — which rules out lifting busybox-lineage utilities or Linux userland code for
  Phase 4. Concepts, ABI numbers and struct layouts are free to take; prose and
  implementation are not. Anything new linked into a shipped binary needs a
  `NOTICE.md` entry. Fonts stay runtime-loaded.
- **Ratchets are measurements, not numbers.** Every phase here grows the stack,
  quota, lockdep and test-count pools. Re-measure with each gate's
  `--emit-allowlist` in the same commit and say which change added the delta.
- **`just boot`'s `verity=require` keeps meaning what it says.** The shipped
  image stays v1-verified and read-only. Everything this plan makes writable is
  a different medium.

---

## Phase 0 — A dev loop that keeps what you wrote

**Outcome:** `just boot-persist` is a machine whose state survives, including a
rude QEMU exit, and the host build never destroys guest data.

This phase is first because it is broken *now*, and because every measurement in
Phase 1 onward is worthless on a root that silently reverts to RAM.

### Root cause (proven, not inferred)

A writable ext2 mount stamps `s_state = EXT2_ERROR_FS` at mount time by design
(`fs/src/ext2_vfs.rs:700-711`, `fs/src/ext2/mod.rs:559-565`) and clears it only
in `mark_filesystem_clean` (`fs/src/ext2_vfs.rs:862-903`), whose only non-test
caller is `boot/src/shutdown.rs:35` on `power::shutdown`. `boot-persist` runs
QEMU interactively: closing the window or Ctrl-C never runs `/bin/halt`, so the
image stays dirty. On the next boot `mount_read_only_reason` returns
`NotCleanlyUnmounted` (`fs/src/ext2/mod.rs:474-476`); the 5 s flusher has already
drained the log, so `attach_journal` replays nothing and the read-only latch
stands (`fs/src/ext2_vfs.rs:574-580`); `root=auto` then treats a read-only disk
exactly as it treats no disk and boots the initramfs, demoting the image to
`/mnt` (`boot/src/boot_services.rs:108-135`). The disk is intact; `/` is a fresh
RAM copy, so nothing the developer does persists.

Observed on the checked-in artifact and two boots of the current ISO:

```
$ dumpe2fs -h fs/assets/ext2-persist.img | grep state
Filesystem state:         not clean with errors
$ e2fsck -fn fs/assets/ext2-persist.img; echo $?
0                                     # the host preserve gate's oracle says "fine"
# boot 1, dirty image:
ext2: MOUNTING READ-ONLY — the image was never marked clean …
ROOTFS: unpacked 26 initramfs entries (6145224 bytes) into RAM root
VFS: mounted ext2 at /mnt (secondary, read-only)
# after `e2fsck -fy`, boot 2:
ROOTFS: root=disk — / is the ext2 disk, and what it holds persists
VFS: mounted / (ext2, read-write), /tmp (ramfs), /dev (devfs)
```

The missing `root=` on `boot-persist`'s cmdline is **not** the bug: the unset
default is `ROOT_AUTO` (`boot/src/early_init.rs:508-510`), which is what this
recipe wants. The v2 (`VERITY=rw`) trailer is **not** the bug either: it permits
writes end to end (`fs/src/verity.rs:326-338`).

### Workstream 0.1 — Mark the image clean while it is idle (guest, **S**)

`mark_filesystem_clean` already computes exactly the right predicate — no dirty
blocks, no unbarriered writes, no dirty superblock, empty journal
(`fs/src/ext2_vfs.rs:874-878`). Call it from the flusher when that predicate
holds, and re-stamp on the next mutation: `mark_dirty_on_disk` is already
idempotent and returns early when the state is already `EXT2_ERROR_FS`
(`fs/src/ext2/mod.rs:559-565`), so only the first write after an idle window
pays a superblock write. The ordering obligation is unchanged and load-bearing:
the dirty stamp must reach the device *before* any mutation it covers.

A rude exit then loses at most the writes of the last idle window, instead of
converting the image into one that never mounts writable again.

### Workstream 0.2 — Never let the host wipe a preserved image (host, **S**)

- `image_is_preservable` uses `e2fsck -fn` as its only oracle
  (`scripts/build_fs_image.sh:129-132`), which `scripts/check_fs_image.sh:10-13`
  documents as blind to the dirty bit — so the latch is sticky and silent. Add
  the `dumpe2fs -h` state assertion, reusing `check_image` rather than writing a
  second copy.
- When a `PRESERVE_FS_IMAGE=1` image fails any check, **refuse and exit
  non-zero** with the `e2fsck -fy` command to run. Today the fallback is
  `rm -f "$IMAGE_PATH"` + `mkfs` announced by one line inside a noisy build
  (`scripts/build_fs_image.sh:172-176`) — that is the reported "it overwrites my
  image". Move the destructive branch behind an explicit `just boot-persist-reset`.
- Gate the `/.journal` superblock zeroing (`scripts/build_fs_image.sh:216-219`)
  on a clean image: on a dirty one the log may hold the only copy of committed
  metadata, and the refresh destroys the evidence the kernel would have replayed.
- Carry the existing attested bitmap forward AND-ed with the fresh one instead of
  regenerating all-ones (`scripts/gen_verity.py:97-107`), so a block the guest
  rewrote stays un-attested across rebuilds. Today every `boot-persist`
  silently re-blesses whatever the guest wrote, contradicting
  `fs/src/verity.rs:166-167`.

### Workstream 0.3 — Stop the host refresh from eating guest work (host, **S**)

`_fs-image-persist` runs on every `boot-persist` and unlinks + rewrites every
`/bin/*` and `/sbin/init` whenever any userland hash changes. Carve a `/home`
(and keep `/var`) that the refresh never touches, and say in the recipe's
`[doc]` string that `/tmp` is RAM and that `shutdown` — not the window's X — is
what commits.

### Workstream 0.4 — Give the machine room and a way in (host, **S**)

`FS_IMAGE_SIZE=32M`, `FS_JOURNAL_SIZE=4M`, `QEMU_MEM=512M`. A workspace disk
needs GB. Add a `DISK_SIZE` knob and a second virtio-blk device
(`scripts/qemu_run.sh:452-457` already parameterises disk1) so the workspace is
separate from the root, and raise `QEMU_MEM` for the persist recipe.

**Phase 0 exit criteria:**
- `just boot-persist`, write a file, close the QEMU window, `just boot-persist`
  again — the file is there, and the boot log says `ROOTFS: root=disk`.
- A dirty or damaged persist image is never silently rebuilt; the script says
  what to run.
- `just test` green; `check-fs-image`, `test-persist` and the four boot-log
  ratchets re-measured.

---

## Phase 1 — Run a large program at all

**Outcome:** a 200 MB statically linked binary execs, maps its inputs lazily,
and can address several GB. Until this lands nothing else in the plan is
*measurable*, because no toolchain-sized process can start.

### Workstream 1.1 — Demand-paged exec (**M**)

`do_exec` stages the whole file into one `KVec<u8>` (`core/src/exec/mod.rs:463`)
under a 1 MiB slab ceiling, with a nominal `EXEC_MAX_ELF_SIZE` of 16 MiB that is
unreachable. The largest binary the tree has ever run is 726 KB
(`builddir/shell.elf`), so the ceiling has never been observed. Replace the
staging buffer with a chunked read straight into the target VMA — the loader
already copies per page in `load_segment_pages` — and raise
`MAX_TOTAL_MAPPED_SIZE` (256 MiB) and `MAX_LOAD_SEGMENTS` (16) in
`mm/src/elf.rs:56-59`.

### Workstream 1.2 — A sleepable fault path (**L**)

The page-fault handler runs on IST4 with interrupts off and cannot sleep, which
is why file mappings are eagerly populated and capped at 16 inodes / 4 MiB
machine-wide (`fs/src/filemap.rs:67-77`, `core/src/syscall/memory_handlers.rs:52`),
and why `mmap` refuses any file whose size is not page-aligned. `rustc` maps
every `.rlib` and `.rmeta` it reads. This is the single largest kernel item in
the plan and everything about toolchain I/O depends on it.

### Workstream 1.3 — Address-space arithmetic that fits a compiler (**M**)

In the order they bite: `Pages` quota 65536 = 256 MiB of VA, enforced by default
(`abi/src/quota.rs:183`); `brk` ceiling at VA 0x4000_0000 and eagerly populated
(`mm/src/memory_layout_defs.rs:84`, `mm/src/process_vm.rs:2185-2229`); a fixed
1 MiB user stack with no growth (`memory_layout_defs.rs:88`) against rustc's
8 MiB main thread; `mprotect` that cannot split a VMA and rewrites the whole
VMA's protection (`process_vm.rs:3005-3021`); `fork`'s per-page snapshot `KVec`
that `.expect()`s and therefore panics the kernel above ~170 MiB resident in one
VMA (`process_vm.rs:3092-3126`); page tables charged to nobody and freed only at
`VmSpace::drop`; all user mappings 4 KiB. Add an RSS notion so a VA charge means
something, and give OOM a disposition other than "kill the faulter" / panic.

### Workstream 1.4 — Exec's argument surface (**S**)

`EXEC_MAX_ARGS`/`EXEC_MAX_ENVS` are 32 each (`core/src/exec/mod.rs:41-42`);
cargo drives rustc with 40–100+ arguments. Bound by total bytes, not count.

**Phase 1 exit criteria:** a purpose-built 200 MB test binary that mmaps a
1 GB file and touches 4 GB of anonymous memory runs to completion under
`just boot-persist`, and `fork` of it does not panic the kernel.

---

## Phase 2 — The POSIX floor a build system stands on

**Outcome:** a program can find its files, learn whether they changed, spawn
children and know how they died.

### Workstream 2.1 — Path resolution (**M**)

Nothing in the tree dereferences a symlink during lookup
(`fs/src/vfs/path.rs:38-63`); `canonicalise` rejects every relative path
(`fs/src/vfs/canon.rs:31-33`) and no resolver consults the cwd the task already
stores; `MAX_NAME_LEN` is 32 and `MAX_PATH_LEN` 256 (`fs/src/lib.rs:4-5`) while
ext2 itself allows 255. `libcore-<hash>.rlib` exceeds 32 bytes; a registry
source path exceeds 256. Every rustup and cargo layout is symlinks. Nothing
downstream matters while `open("src/main.rs")` returns `EINVAL`.

### Workstream 2.2 — `stat` that carries time (**S**, ABI break)

`UserFsStat` is `{type_, _pad, size: u32}` (`abi/src/fs.rs:96-100`); slibc
hardcodes `st_mtime = 0`. The ext2 inode *is* stamped
(`fs/src/ext2/time.rs:19-23`) and the value is discarded at the ABI. Cargo's
entire fingerprint model is mtime-based, so today it either rebuilds everything
or nothing. Widen to `ino`/`mode`/`nlink`/`uid`/`gid`/`u64 size`/`mtim`/`ctim`,
add `utimensat`, and take the break before more userland is written against the
current shape. Prerequisite: `clock_settime` + a real RTC read, because
`fs/src/ext2/time.rs` declines to stamp at all when the wall clock is unset.

### Workstream 2.3 — Process results (**S**)

`waitpid` is `(target, flags)` returning a raw exit code
(`core/src/syscall/process_handlers.rs:268-330`); slibc passes `status` into the
flags slot and never writes it, so `std`'s `ExitStatus` is always 0 — **every
failed rustc currently reports success**. Add the status pointer and the
`(code<<8)|sig` encoding, then `exit_group` (today `exit` kills one task and
leaves sibling threads running), thread-group `kill` fan-out, and
`SIGSTOP`/`SIGTSTP`/`SIGCONT` (silently dropped at
`core/src/syscall/signal.rs:603`, so Ctrl-Z on a build does nothing).

### Workstream 2.4 — Threads that behave (**M**)

`futex` matches only bare ops 0 and 1, so every `FUTEX_PRIVATE_FLAG` call is
`ENOSYS` (`process_handlers.rs:800-812`) and timeouts are relative
milliseconds. `CLONE_SIGHAND` is validated and then not implemented — threads
get private handler tables. A user fault never becomes a deliverable signal
(`slopos-ostd/src/task/borrowed.rs:141-150`), so there is no catchable SIGSEGV
and no `sigaltstack`, and std's stack-overflow guard cannot run. `fork` in a
multithreaded process allocates in the child before `execve`
(`slibc/std_pal/process/slopos.rs:180-205`) against a single global malloc
spinlock — route std's spawn through the existing `spawn_path` syscall instead.

### Workstream 2.5 — The syscalls cargo reaches for (**M**)

`flock`/`fcntl(F_SETLK)` (cargo refuses to run without a package-cache lock),
`link` (cargo hardlinks artifacts), the `*at` family, `getdents64` on an fd
(and therefore `fsync` on a directory), `pread`/`pwrite`/`readv`/`writev`,
`fchmod`, `getrandom` above 256 B, `uname`, `CLOCK_*_CPUTIME_ID`. Wire the
std PAL to the syscalls that *already exist* first — `symlink`, `readlink`,
`truncate`, `chmod` are implemented in `slibc/src/pal/slopos.rs:235-256` and
simply return `unsupported` in `slibc/std_pal/fs/slopos.rs:713-723`; and
`Command::current_dir` is stored and never applied.

**Phase 2 exit criteria:** a hand-written build driver, running in-guest,
compiles a multi-file project with a stub compiler, correctly skips unchanged
inputs on the second run, and reports a child's signal death.

---

## Phase 3 — Storage and capacity for a real tree

**Outcome:** a multi-GB working tree with hundreds of thousands of files, at a
throughput where a build finishes.

### Workstream 3.1 — Size the filesystem from the medium (**M**)

Fixed at appliance scale: the block cache is 512 entries / 2 MiB
(`fs/src/ext2/cache.rs:26`), never derived from the volume — at 16 GiB the group
bitmaps alone need 257 of those 512 slots, and every allocation then evicts a
bitmap it is about to need. The journal is 4 MiB regardless of image size, with
`resident_slot` a linear backward scan (`fs/src/ext2/journal.rs:255-267`), so
simply growing it makes every miss quadratic. `allocate_searching` restarts at
bit 0 of each group with no hint. Verity's hash array is 4 bytes per block in
one contiguous `KVec` — 64 MiB of kernel memory for a 64 GiB volume. Directory
lookup is a linear scan with no htree, so `target/debug/deps` with 20 k entries
makes the build O(n²).

### Workstream 3.2 — Make a write cost what it writes (**M**)

User I/O is staged in 4096-byte chunks (`abi/src/io.rs:5`) and each chunk is a
separate ext2 transaction (`fs/src/vfs_file_ops.rs:373-397`), whose data also
goes into the log when it is small. Linking a 60 MB object is ~15,000
transactions against a 1023-slot log, forcing hundreds of `checkpoint_journal()`
calls, each an inline whole-filesystem `sync()` under the mount lock. Batch the
transaction over a multi-block range and replace the inline sync with the
chunked `sync_step`.

### Workstream 3.3 — More than one filesystem (**L**)

There is exactly one ext2 instance, bound at boot, behind one global mutex
(`fs/src/ext2_vfs.rs:57-63`); `mount(2)` with `fstype=ext2` can only re-place
that instance (`core/src/syscall/fs/mount_handlers.rs:103-118`). No second disk,
no separate `/home`, no scratch volume, and every filesystem operation on the
machine serialises — `-j16` degenerates toward one core. Also: `/tmp` is a ramfs
with a 16 MiB per-file cap and 4096 inodes, and there is no swap anywhere.

### Workstream 3.4 — The block layer (**M**)

`virtio_blk` takes a global `io_lock`, bounces through a fresh 4 KiB buffer and
sleeps, one request at a time machine-wide, with a queue depth of 1 against a
128-entry virtqueue (`drivers/src/virtio_blk.rs:42-51,441-455`). Every failure
maps to `InvalidBuffer`, and eight timeouts permanently quarantine the device.
Scatter-gather into the caller's pages, allow concurrent slots, and give errors
real variants with bounded retry.

**Phase 3 exit criteria:** a 16 GiB image mounts in bounded time, holds a
checked-out copy of this repository plus a toolchain sysroot, and sustains a
measured sequential write rate recorded as a new ratchet.

---

## Phase 4 — A workbench you can type in

**Outcome:** you can edit a file, search a tree, run a script, and read the
output — without a Linux host.

### Workstream 4.1 — Utilities that are executables (**M**)

`ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `diff`, `env`, `kill`, `ps` exist only
as shell builtins (`userland/src/apps/shell/builtins/`); `/bin` holds 17 GUI and
network binaries. Anything that spawns a tool directly gets `ENOENT`. Give the
existing builtins `main`s, then write the absent set: `grep` `find` `sed`
`sort` `uniq` `tr` `cut` `xargs` `which` `test`/`[` `printf` `basename`
`dirname` `mktemp` `tar` `gzip` `patch` `cmp` `install` `sha256sum` `nproc`
`stty` `less`. Fix the semantics that are wrong rather than missing: `sleep`
takes milliseconds, `kill` sends only SIGKILL, `diff` cannot produce a patch,
`rm`/`cp` have no `-r`, `mkdir` has no `-p`.

### Workstream 4.2 — A shell that can drive a build (**L**)

No `if`/`while`/`for`/`case`/functions, no command substitution, no here-docs,
no globbing (`userland/src/apps/shell/`). Structural caps: 8 pipeline stages,
64 argv words, 128-byte paths. `fg`/`bg` cannot resume a stopped job because
there is no `Stopped` state.

### Workstream 4.3 — A terminal an editor can use (**M**)

`encode_key` emits arrows, Home, End and Delete only
(`terminal-core/src/input.rs:230-282`): no F1–F12 (the keycodes exist and are
dropped), no Alt-prefixing, no modified arrows, no `CSI Z`, and PageUp/PageDown
never reach the PTY. No mouse reporting, no DA/DSR replies. The font atlas
covers ASCII + Latin-1, so box-drawing and non-Latin source render as diamonds.

### Workstream 4.4 — An editor (**M**)

Write one — not because C is foreclosed (it is not; see Workstream 5.6), but
because nothing upstream is reachable *before* a C frontend exists, and because
an editor is where a desktop OS earns its character. Highlighting does not have
to wait for C either: `syntect` with the pure-Rust `fancy-regex` backend is a
Rust-only path to TextMate grammars. Start against the existing terminal; the
GUI version needs a real multi-line text widget, which `appkit` does not have (a
single-line `text_field`, and a byte-oriented text API). helix comes back onto
the table once 5.6 compiles tree-sitter.

**Zed is not a roadmap item.** It needs wgpu → Vulkan (no GPU driver, and the
Vulkan loader is itself a `dlopen` ICD architecture), tree-sitter, a live C++
dependency set, and a build performed by a toolchain that does not exist yet.
Every one of those is a separate multi-month project whose payoff is one editor.

**Phase 4 exit criteria:** a shell script in the guest checks out, greps,
edits and archives a source tree, driven from a terminal running a native
editor.

---

## Phase 5 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

This phase is **XL**. **Decided: the Rust toolchain is Rust-hosted** — rustc
with the cranelift backend and a Rust linker, no LLVM. Read that as a statement
about *who compiles Rust*, not about which languages SlopOS supports: declining
LLVM declines a **C++** toolchain port (templates, exceptions, libc++/libc++abi,
the Itanium ABI), which is the expensive part, and says nothing about C.
A C toolchain written in Rust is a separate and wanted track — Workstream 5.6.
The cost of this decision is upstream work: cranelift-only rustc bootstrap does
not currently work (it did in 2020 and regressed), cranelift emits no debug
info, and `wild` is explicitly not production-grade. Redox took the other road —
relibc, GCC, binutils, then rustc in January 2026 on its third attempt — which
is the reference class this decision is *declining*, with eyes open.

### Workstream 5.1 — The ABI question (still open — see Open decisions)

Every gap in Phases 1–3 is a Linux-ABI-shaped hole. SlopOS's numbering is
bespoke and append-only (`yield=0, exit=1, write=2, read=3`,
`abi/src/syscall/numbers.rs`) while the *constants inside* the calls are already
Linux-valued — errno, `O_*`, `PROT_*`, `MAP_*`, `CLONE_*`, termios ioctls. The
two reference designs split on architecture, not taste:

- **Asterinas** — the framekernel whose AD-1/AD-2 discipline this tree already
  follows — is **Linux ABI-compatible by construction**: 210+ Linux syscalls,
  Linux numbers, Linux struct layouts, implemented entirely in *safe* Rust on
  OSTD, with a 14% memory-safety TCB and LMbench parity. Being Linux-ABI does
  not make a kernel a sloppy Linux; Asterinas is the standing proof, and it is
  the same architecture class as SlopOS.
- **Redox** is a microkernel and deliberately *not* Linux-ABI: its kernel
  interface is intentionally unstable and minimal (Plan 9 schemes), and POSIX
  lives in userspace in relibc/redox-rt. The stable ABI boundary is in
  userspace. The result is source compatibility, not binary compatibility —
  every port is a source port, which is precisely why rustc took years.

SlopOS is a framekernel, not a microkernel: services live in the kernel, in one
address space, behind one syscall table. That is Asterinas's shape, and it is
the shape for which a Linux ABI is cheap. What is bespoke here is *numbering and
a handful of struct layouts* — the two parts of an ABI that carry no design
value. `AGENTS.md:100-105` already settles the licensing half: "ABI numbers,
`errno` values, ioctl codes, struct layouts … carry no copyright, which is why
the ABI-compatibility work is sound."

The counterweight is real and must be priced: ~190 slots becomes ~350, the
capability classification that `core/src/syscall/handlers.rs` proves total has
to cover all of them, and Linux's warts (32 signals, the wait-status encoding,
`stat` padding, ioctl numbering) become permanent. Nothing about adopting the
interface obliges adopting Linux's implementation, architecture or policy —
the framekernel quarantine, the capability authority, the Verus proofs, the
ratchets and the retractable filesystem are all things the ABI cannot touch.

### Workstream 5.2 — A target that can be a host (**L**)

A JSON target can never be a rustc host. `scripts/patch_std.sh` is 715 lines of
sed/perl that mutates the *live rustup sysroot's* std sources in place — an
excellent bootstrap hack and a non-self-hostable one. A native rustc needs
`x86_64-unknown-slopos` compiled into `rustc_target`, a `libc` crate port, and
std upstreamed or carried in a pinned fork. That also kills `restricted_std`,
which currently forces `#![feature(restricted_std)]` into 59 files and makes
every unmodified crates.io crate uncompilable.

### Workstream 5.3 — A Rust codegen path for a `no_std` kernel target (**L**)

Decided pure Rust, so the C floor is out of scope and the risk moves into
cranelift's coverage of *this* tree's kernel target. Spike this first, before
anything else in Phase 5, because a negative answer changes the decision:
`targets/x86_64-slos.json` requires soft-float with `-sse` and `rustc-abi:
softfloat`, safestack, custom `link_section`s, naked functions, and
`-Zemit-stack-sizes` — the last is what `check_stack_sizes.sh` reads, so a
backend that does not emit `.stack_sizes` silently disarms the S-5 gate. Naked
functions are backend-independent now (emitted as global asm) and inline asm is
largely stable in cg_clif; soft-float, safestack and `.stack_sizes` are
unverified. The linker is the second half: `wild` or a linker written here, and
it must honour `-T link.ld` with the registry sections
`check_registry_sections.sh` polices.

Note the split this permits: the *release* kernel can keep being built by an
LLVM rustc on a host for as long as cranelift's codegen quality matters, while
the self-hosted loop builds the dev kernel. Self-hosting does not have to mean
every artifact is self-built on day one.

### Workstream 5.4 — Dynamic linking is mandatory (**L**)

Not optional, and pure Rust does not dodge it: `slopos-ostd-derive` is a
proc-macro crate (`#[derive(SlotFields)]`) and `paste` is another, and rustc
loads proc-macro crates as host **dylibs** at runtime. Building SlopOS on
SlopOS therefore requires `PT_INTERP` + `dlopen` — today `PT_INTERP` is rejected
(`mm/src/elf.rs:591`), the target is `relocation-model: static`, and every
binary is fixed at 0x400000. The only escapes are writing an out-of-process
macro server (novel work) or deleting proc-macro use from the workspace. This
also brings dynamic TLS (`__tls_get_addr`, DTV), which does not exist.

### Workstream 5.5 — Getting code in and out (**S** for the goal, **M** beyond it)

Off the critical path, and this is a real scope reduction: `Cargo.lock` holds 47
entries of which only nine are third-party (`bitflags gimli libm limine paste
proc-macro2 quote syn unicode-ident unwinding`). Vendoring that is trivial, so
**building SlopOS on SlopOS needs no network at all** — no TLS, no crates.io, no
`git`. Those remain wanted for a general dev machine (there is no TLS anywhere:
`curl` rejects `https://` outright; DNS is one query at a time machine-wide; the
TCP window is capped at 32 KiB by a fixed buffer), but they are Phase 5+
comfort, not a blocker for the goal.

### Workstream 5.6 — A C toolchain, written in Rust (**M**/**L**, not on the critical path)

C is not foreclosed by the pure-Rust decision, and closing it off would be a
mistake: C is the interoperability floor of the world, and every piece of it can
be built in Rust here.

- **A C library.** `slibc` already *is* a C ABI — ~200 `#[unsafe(no_mangle)]`
  Rust functions. What is missing is linkability: `crate-type = ["staticlib"]`
  alongside `rlib`, generated `include/*.h`, and a `crt0.o` emitted from the
  existing `_start` + `__slibc_start` pair (`userland/src/lib.rs:20-34`,
  `slibc/src/crt/mod.rs:97`), plus libm (wrap the `libm` crate the tree already
  vendors for `font/`), `setjmp`/`longjmp`, `opendir`, `qsort`, `strerror` and
  the `<time.h>` calendar. **S/M**, and it is the single gate that turns "can a
  C program be built here" from *no* into *yes*.
- **A C frontend.** A C99 compiler written in Rust emitting cranelift IR, reusing
  the *same* backend and the *same* Rust linker as the Rust toolchain — the
  marginal cost is a frontend, not a second toolchain. `saltwater` (formerly
  `rcc`) is the existence proof of exactly this shape, though unmaintained since
  February 2025, so treat it as a reference design rather than a dependency.
  **M** for a C99 frontend that compiles simple, generated, switch-heavy C;
  **L** for one that survives real-world C (GNU extensions, `__builtin_*`,
  bitfields, VLAs, inline asm, `setjmp` interaction).
- **What this buys.** tree-sitter (a ~10 kLoC C99 runtime plus generated
  parsers, which is close to the easiest interesting C target there is) and
  therefore helix; `cc`-crate build scripts across the ecosystem; and every C
  library worth having. `[INFERENCE]` on tree-sitter's exact dialect needs —
  verify against its sources before committing.
- **What it still does not buy.** C++. Templates, exceptions and unwinding,
  name mangling, the Itanium ABI, libc++/libc++abi — an order of magnitude past
  a C frontend, and the reason the LLVM route was priced as it was. Nothing in
  this plan needs C++, and this workstream does not change that.

Order it after Phase 5's Rust loop closes: the C frontend is much cheaper to
write once cranelift and the linker are already known-good on this target.

**Phase 5 exit criteria:** in-guest `cargo build` of this repository's kernel
produces an ELF byte-identical in behaviour to the host build, verified by
booting it.

---

## Phase 6 — Install what you built

**Outcome:** the guest writes a bootable medium and reboots into its own kernel.

Nothing here exists. `write` on a `/dev` block node returns `ReadOnly`
(`fs/src/devfs/mod.rs:309-311`) and reading one needs `TASK_FLAG_SYSTEM`; there
is no FAT/vfat support anywhere, so an ESP cannot be written; partition tables
are parse-only (`fs/src/partition.rs`); Limine is fetched and installed by host
scripts; QEMU boots `order=d` (CD only) with throwaway OVMF vars. Needed: a
writable block path, FAT32 write, a bootloader installer or a direct EFI stub, a
`limine.conf` editor, `SYSCALL_REBOOT` (exists) landing on the new image, and
A/B slots with rollback. `AGENTS.md:324` currently forbids exactly this
operation and needs a scoped exception for the guest's own ESP.

**Phase 6 exit criteria:** `just boot-persist`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 7 — Bare metal (not committed)

Out of scope for the current goal, which ends at Phase 6 in QEMU. Recorded so
the cost is known: no NVMe and no AHCI (virtio-blk is the only storage driver,
so a real machine has no disk); no USB at all, so a laptop without PS/2 has
**no keyboard** (`plans/usb-xhci.md`); PCI is ECAM-only and *panics* without
MCFG; x2APIC is forcibly disabled so machines with APIC IDs > 254 do not boot;
no real NIC; no ACPI SCI/GPE runtime, so no power button, no lid, no thermal
events during a multi-hour build; no CPU frequency management; no real RTC (the
wall clock comes from Limine once and is never corrected); EFI runtime services
are `ResetSystem` only; COM1 port I/O is the only serial, so the debug channel
and the KTAP transport vanish exactly when bare-metal debugging starts.

---

## Open decisions

- [ ] **Linux ABI: adopt the interface, or stay bespoke?** The one decision
      still open, and the highest-leverage one here. See Workstream 5.1 for the
      Asterinas/Redox evidence. Recommendation: **renumber once, now, onto Linux
      numbers and Linux struct layouts, as the single syscall table** — not a
      second surface. The userland is entirely first-party and rebuilt from
      source every build, so renumbering is nearly free today and compounds in
      cost with every binary written against the current numbers. SlopOS-only
      calls (SlopRing ops, seat, W/L, fate) go in a private high range exactly
      as Linux does for its own extensions. What SlopOS keeps is everything that
      actually makes it not-Linux: the framekernel quarantine, capability
      authority per syscall, Verus proofs, KernMiri, the ratchets, the
      retractable filesystem.
- [ ] **Does the dev root stay attested?** A machine that rewrites `/usr` while
      building itself un-attests exactly the blocks it changes. Decide which
      paths stay verified and what `verity=require` asserts for a workbench.
- [ ] **How does source get in before the guest can fetch it?** A host-built
      disk image, a 9p/virtiofs mount, or a plain TCP transfer — each is a
      different amount of throwaway work. Cheapest: a second virtio-blk disk
      built by the host with the vendored tree on it (Workstream 0.4).

**Decided.** Rust toolchain: Rust-hosted (cranelift + a Rust linker), no LLVM
and no C++ toolchain port; time is not the constraint. C is *not* excluded — a
C library and a Rust-written C frontend are Workstream 5.6, off the critical
path. Scope: the full in-guest loop, Phases 0–6, in QEMU; bare metal is not
committed. Identity: single-user, uid 0, permanently — no persistable
principal, so file ownership and a medium-resident quota ledger stay out of
scope and `stat`'s uid/gid fields exist for layout only.

---

## Touch list (current paths — verify before editing)

- `scripts/build_fs_image.sh:107-176,216-219` — preserve gate, refresh branch,
  journal zeroing (Phase 0).
- `scripts/gen_verity.py:97-107` — attested-bitmap regeneration (Phase 0).
- `justfile:100-102,188-197` — `_fs-image-persist`, `boot-persist` (Phase 0).
- `scripts/qemu_run.sh:445-466` — disk attachment, boot order (Phases 0, 6).
- `fs/src/ext2_vfs.rs:862-903,922-960` — `mark_filesystem_clean`, the flusher
  (Phase 0).
- `core/src/exec/mod.rs:41-43,458-490` — argv caps, ELF staging (Phase 1).
- `mm/src/elf.rs:56-59,591` — image caps, `PT_INTERP` (Phases 1, 5).
- `mm/src/process_vm.rs:2185-2229,3005-3021,3092-3126` — brk, mprotect, fork
  snapshot (Phase 1).
- `mm/src/memory_layout_defs.rs:84-90` — heap ceiling, stack size (Phase 1).
- `abi/src/quota.rs:178-186` — `Pages`, `KernelMeta` defaults (Phase 1).
- `fs/src/filemap.rs:67-77` — file-mmap caps (Phase 1).
- `fs/src/vfs/path.rs:38-63`, `fs/src/vfs/canon.rs:31-33`, `fs/src/lib.rs:4-5` —
  symlinks, relative paths, name/path limits (Phase 2).
- `abi/src/fs.rs:96-100` — `UserFsStat` (Phase 2, ABI break).
- `core/src/syscall/process_handlers.rs:268-334,800-812` — waitpid, futex
  (Phase 2).
- `core/src/syscall/signal.rs:603,737-739` — stop/cont, siginfo (Phase 2).
- `slibc/std_pal/fs/slopos.rs:446-724`, `slibc/std_pal/process/slopos.rs:151-215`
  — unwired std PAL entry points (Phase 2).
- `fs/src/ext2/cache.rs:26`, `fs/src/ext2/journal.rs:207-267`,
  `fs/src/verity.rs:524-552` — fixed sizing (Phase 3).
- `fs/src/vfs_file_ops.rs:373-397`, `abi/src/io.rs:5` — per-4 KiB transactions
  (Phase 3).
- `core/src/syscall/fs/mount_handlers.rs:103-118`, `fs/src/ext2_vfs.rs:57-63` —
  the single ext2 instance (Phase 3).
- `drivers/src/virtio_blk.rs:42-51,441-455,800-853` — request path, error
  variants (Phase 3).
- `userland/src/apps/shell/`, `terminal-core/src/input.rs:230-282`,
  `font/src/lib.rs:29-48` — shell, key encoding, glyph coverage (Phase 4).
- `scripts/patch_std.sh`, `targets/x86_64-slos-userland.json`,
  `userland/userland.ld:44-50` — the std/target/unwinding triangle (Phase 5).
- `fs/src/devfs/mod.rs:309-323`, `fs/src/partition.rs` — writable block nodes,
  partition writing (Phase 6).
- `AGENTS.md:324` — the QEMU-only execution boundary, which forbids exactly the
  Phase 6 install operation and needs a scoped exception.
