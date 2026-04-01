# Spawn Page Table Visibility Race — `copy_from_user` Fails on SMP

## Symptom

Intermittent "shell: surface init failed" on ~40-60% of boots. The shell connects to the compositor successfully, the compositor accepts and sends OutputInfo, but the shell's `poll(POLLIN, 10000)` syscall returns -22 (EINVAL) immediately on every call, causing `ensure_output_info()` to report Timeout.

## Root Cause (Confirmed)

`copy_from_user` inside `syscall_poll` fails because `validate_user_pages` cannot find the shell's user stack mapping in the page directory. The shell's process ID is correct (PID=3), `require_process_id` succeeds, the poll handler body runs, but the `try_or_err!(ctx, copy_from_user(user_ptr))` at `poll_ioctl_handlers.rs:93` returns an error — the UserPollFd struct on the shell's stack is in unmapped memory according to `validate_user_pages`.

This is a page table visibility race in `spawn_program_with_attrs` (`core/src/exec/mod.rs`). The page tables modified by `do_exec` on the spawning CPU (CPU 0) are not always visible to `validate_user_pages` running on the shell's CPU (CPU 1).

## Why It's Intermittent

The shell is spawned on CPU 0 via `spawn_program_with_attrs`, which calls `do_exec` to load the ELF and set up the stack. Then `schedule_new_task` enqueues the task on a different CPU (CPU 1). If CPU 1 picks up the task and the shell makes a syscall before the page table modifications from `do_exec` are fully visible through `process_vm_get_page_dir` + `paging_is_user_accessible`, `copy_from_user` fails.

On boots where CPU 1 happens to see the page tables (cache coherency timing, or the shell's first syscall is delayed enough), everything works.

## Detailed Trace of the Failure

### The Spawn Path (CPU 0)

```
spawn_program_with_attrs() [core/src/exec/mod.rs:98-208]
  task_create()                    → creates process VM, page dir D1, sets CR3 = D1.pml4_phys
  set_status(Blocked)              → reset to Blocked for post-create writes
  do_exec(process_id, path, ...)   → calls:
    process_vm_load_elf_data()     → maps ELF sections into page directory
    process_vm_reset_stack()       → resets stack mappings
    setup_user_stack()             → maps user stack, writes argv/envp
  update task: entry_point, rip, rsp, fs_base, InterruptFrame on kstack
  clone fd table from parent
  set pgid, sid, controlling_tty
  set_status(Ready)                → Release ordering, publishes all writes
  schedule_new_task(task_info)     → enqueues on CPU 1's run queue
```

### The Shell's First Syscall (CPU 1)

```
scheduler dequeues shell task → context switch → loads CR3 from task.context.cr3
shell entry point runs → protocol_client::init() → Sys::socket() + Sys::connect() [these work!]
Surface::new() → ensure_output_info() → wait_recv() → poll_readable() → raw_poll()

syscall_handle() [core/src/syscall/dispatch.rs:14]
  task = scheduler_get_current_task()          → shell task ptr
  (*frame).rax = ERRNO_EINVAL                  → sentinel (-22)
  pid = (*task).process_id                     → 3 (correct!)
  set_syscall_process_id(pid)                  → stores pid=3 in per-CPU atomic
  syscall_lookup(108)                          → finds syscall_poll handler
  handler(task, frame)                         → enters syscall_poll

syscall_poll [core/src/syscall/fs/poll_ioctl_handlers.rs:62]
  require_process_id()                         → Ok(3) (succeeds!)
  args.arg0 != 0 && nfds <= 256               → passes validation
  UserPtr::try_new(base_ptr)                   → Ok (address looks valid)
  copy_from_user(user_ptr)                     → FAILS!
    current_process_dir()
      current_process_id()                     → 3
      process_vm_get_page_dir(3)               → returns page directory pointer
    validate_user_pages(user_addr, size, dir)
      paging_is_user_accessible(dir, addr)     → 0 (NOT MAPPED!)
    returns Err(NotMapped)
  try_or_err! returns ctx.err()                → writes -22 to frame.rax (same as sentinel)
  handler returns
```

### Why the Stack Isn't Mapped

The page directory returned by `process_vm_get_page_dir(3)` either:

1. **Points to the ORIGINAL page directory from `task_create`** which doesn't have the ELF/stack mappings added by `do_exec`. This would happen if `do_exec` creates a NEW page directory and updates the process VM table entry, but the old `task.context.cr3` still points to the original.

2. **Points to the CORRECT page directory** but the page table entries written by `do_exec` on CPU 0 aren't visible to `paging_is_user_accessible` on CPU 1. On x86-64 this should not happen due to cache coherence, but could if:
   - The page directory is walked through uncacheable mappings
   - There's a software caching layer in the process VM subsystem
   - The page table walk function has a bug with newly allocated pages

3. **The page directory pointer is NULL** because `process_vm_get_page_dir(3)` can't find PID 3 in the process VM table. This could happen if there's a lock-free lookup that races with the table being populated.

## Key Observation: Socket Syscalls Work, Poll Doesn't

The shell successfully creates a socket (`Sys::socket`), connects (`Sys::connect`), and sets non-blocking mode (`Sys::fcntl`). These syscalls also use `copy_from_user` (for the sockaddr struct). If they work, the page tables ARE accessible for those calls.

But `poll` fails. The difference: the socket/connect/fcntl syscalls are called during `protocol_client::init()`, while poll is called later during `Surface::new() → ensure_output_info()`. Between init and surface creation, the shell does more work (allocating buffers, etc.) which might grow the stack into a new page that isn't mapped.

**Alternative hypothesis**: The stack pointer used during poll is DEEPER (more function calls) than during socket/connect. If the stack has grown past the initially mapped region, the new stack pages might not be mapped (no demand paging or guard pages).

## Investigation Checklist

### Step 1: Determine if `process_vm_get_page_dir(3)` returns NULL or a valid pointer

Add a diagnostic in `validate_user_pages` (`mm/src/user_copy.rs:79`):
```rust
fn validate_user_pages(user_addr: UserVirtAddr, len: usize, dir: *mut ProcessPageDir) -> Result<(), UserPtrError> {
    if dir.is_null() {
        // DIAGNOSTIC: serial print "validate_user_pages: dir=NULL pid=..."
        return Err(UserPtrError::NoProcess);
    }
    // ...
}
```

If dir is NULL → the process VM lookup failed → investigate `process_vm_get_page_dir`.
If dir is valid → the page walk failed → investigate `paging_is_user_accessible`.

### Step 2: Check if `do_exec` creates a new page directory

Read `process_vm_load_elf_data` in `mm/src/process_vm.rs` (or similar). Check if it:
- Modifies the EXISTING page directory (D1 from task_create) → page walk issue
- Creates a NEW page directory (D2) → CR3 stale issue

If D2 is created:
- Check if `task.context.cr3` is updated after `do_exec` returns
- Check if `process_vm_get_page_dir` returns D1 or D2

### Step 3: Compare the RSP during socket vs poll syscalls

Add a diagnostic that prints `frame.rsp` (the user stack pointer) when copy_from_user fails:
```rust
// In poll handler, on copy_from_user failure:
let rsp = unsafe { (*frame).rsp };
// serial print rsp value
```

Compare with the stack top from `setup_user_stack`. If RSP is far below the mapped stack region, the stack has grown past the mapping.

### Step 4: Check stack mapping size

In `setup_user_stack` (likely in `core/src/exec/mod.rs` or `mm/src/process_vm.rs`), check:
- How many pages are mapped for the user stack?
- Is there a guard page or demand paging mechanism?
- What is the stack top address and how far can it grow?

If the stack is only 1-2 pages (4-8KB), deep function call chains (shell → appkit → protocol → poll_readable → raw_poll) could exhaust it and cross into unmapped territory.

### Step 5: Verify with single-CPU boot

Boot with `QEMU_SMP=1` (single CPU). If the bug disappears, it's a cross-CPU visibility issue. If it persists, it's a page table setup issue independent of SMP.

```bash
QEMU_SMP=1 VIDEO=1 timeout 8 just boot-fast 2>&1 | grep "surface init"
```

### Step 6: Check if paging_is_user_accessible walks the correct directory

In `paging_is_user_accessible` (likely `mm/src/paging.rs`), add a diagnostic that prints:
- The PML4 physical address of the directory being walked
- The virtual address being checked
- Which level of the page walk fails (PML4E, PDPTE, PDE, PTE)

Compare the PML4 physical address with `task.context.cr3` — if they differ, the process VM table is returning a different directory than what the CPU uses.

## Debug Print Mechanism

The kernel serial print that WORKS (confirmed in this session):
```rust
unsafe {
    slopos_utils::ports::serial_write_bytes(
        slopos_utils::ports::COM1,
        b"[TAG] your message\n",
    );
}
```

For printing numbers, use `slopos_utils::numfmt::fmt_u32(value, &mut buf)` which returns a `&[u8]` slice.

The userland TTY print that WORKS:
```rust
let _ = crate::syscall::tty::write(b"[TAG] message\n");
```

**WARNING**: The userland `SYSCALL_WRITE` is a 2-argument syscall `(buf_ptr, buf_len)` — NOT `(fd, buf_ptr, buf_len)`. Using `syscall3` with an fd argument shifts all arguments and produces silent failures. Always use `slopos_slibc::pal::raw::syscall2(SYSCALL_WRITE, ptr, len)`.

## Boot Test Methodology

Fast iteration:
```bash
# Single failure-detecting boot (8s timeout, skip roulette)
VIDEO=1 timeout 8 just boot-fast 2>&1 | grep "surface init failed"

# Batch test (count failures out of N boots)
fails=0; for i in $(seq 1 20); do
  r=$(VIDEO=1 timeout 8 just boot-fast 2>&1 | grep -ac "surface init failed")
  fails=$((fails + r))
done; echo "Failures: $fails / 20"
```

Failure rate with current code: ~40-60% of boots (4-6 out of 10).

## Files Involved

- `core/src/exec/mod.rs:98-208` — `spawn_program_with_attrs`: the spawn path
- `core/src/scheduler/task/task_lifecycle.rs:455-554` — `task_create`: creates process VM + page dir
- `mm/src/user_copy.rs:71-77` — `current_process_dir`: page dir lookup for copy_from_user
- `mm/src/user_copy.rs:79-126` — `validate_user_pages`: software page table walk
- `mm/src/user_copy.rs:128-142` — `copy_from_user`: validates then copies
- `mm/src/process_vm.rs` — `process_vm_get_page_dir`, `process_vm_load_elf_data`, `process_vm_reset_stack`
- `mm/src/paging.rs` — `paging_is_user_accessible`: checks if VA is mapped in a page directory
- `core/src/syscall/dispatch.rs:38-39` — reads `task.process_id` and sets per-CPU provider

## What's Already Fixed (Committed)

- **Poll wakeup race** (commit `fa6ee82`): register-before-check in all poll_fused impls
- **Spawn Blocked→Ready** (same commit): task reset to Blocked before post-create writes, Ready published after all writes with Release ordering

These fixes are correct and should stay.

## RESOLVED: Per-CPU `syscall_pid` Clobbered by Preemption

**Actual root cause**: NOT a page table visibility race. The real bug was in
`copy_from_user`'s process ID lookup.

`syscall_handle()` (dispatch.rs:39) called `set_syscall_process_id(pid)` which
stored the PID in `pcr.syscall_pid` and overrode `CURRENT_TASK_PROVIDER` to
read from that per-CPU field.  When a syscall handler was preempted (e.g., by
`run_bottom_halves()` or `prepare_to_wait()` enabling interrupts), the next
task's syscall on the same CPU overwrote `pcr.syscall_pid`.  On resume,
`copy_from_user` read the WRONG PID, looked up the WRONG page directory, and
`validate_user_pages` failed because the user address was not mapped in the
other process's page tables.

**Diagnosis evidence**: VALIDATE_FAIL showed `pid=2` (compositor) and
`dir=0x...060` for an address that was clearly in pid=3's (shell) stack.
The poll handler's own `pid` variable (from the task struct) was correct
(pid=3), proving the per-CPU lookup diverged from the actual task.

**Fix**: Removed the `set_syscall_process_id(pid)` call from `syscall_handle`.
The scheduler already registers `current_task_process_id` as the provider at
init time (scheduler.rs:1047).  This reads from `scheduler_get_current_task()`
which follows `pcr.current_task` — a value correctly updated on every context
switch.  This is the Linux `current->mm` pattern: always follow the actual
running task, never a stale per-CPU cache.

**Verification**: 60/60 boots pass (0% failure rate, down from ~50%).
All 2279 kernel tests pass.
