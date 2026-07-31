#![feature(restricted_std)]

//! `spawn_path` privilege containment, exercised through the real syscall entry.
//!
//! `syscall_spawn_path` used to hand `SpawnAttrs::flags` to `task_build`
//! verbatim, so every privilege the kernel recognises was a value userland
//! wrote. The kernel-side stest covers the same table against an unprivileged
//! principal; this binary pins the userland-visible `i32` returns and proves the
//! process survives each refusal.
//!
//! The utest runner spawns test binaries with `TASK_FLAG_USER_MODE |
//! TASK_FLAG_SYSTEM`, so **this caller holds `SYSTEM`**. That is not a hole to
//! work around — it is what makes the `SYSTEM` case below load-bearing. The
//! syscall boundary is purely subtractive: it strips privileged bits regardless
//! of what the caller holds, and the privileged bits a child ends up with come
//! from the kernel's program-identity table instead.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point is
// linked into the binary (same requirement as the sibling test bins; without it
// the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use slopos_abi::spawn::{SpawnAttrs, SpawnFdAction};
use slopos_abi::syscall::SYSCALL_SPAWN_PATH;
use slopos_abi::task::{
    TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NEW_PGRP,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE,
};
use slopos_userland::syscall::process;
use slopos_userland::syscall::raw::syscall5;

const EPERM: i32 = -1;
const EINVAL: i32 = -22;
const ENOENT: i32 = -2;

const PRIORITY_HIGH: u8 = 0;
const PRIORITY_KERNEL_IO: u8 = 1;
const PRIORITY_NORMAL: u8 = 2;
const PRIORITY_IDLE: u8 = 4;

/// A path that fails at VFS open, so a request that clears validation is proven
/// by an exec-stage errno with no child ever created.
const MISSING: &[u8] = b"/bin/__no_such_binary__";

/// Issue one `spawn_path` with a raw priority byte and flag word.
///
/// Built by hand rather than through `process::spawn_path_with_actions`, whose
/// typed `TaskPriority` parameter cannot express the tiers under test.
fn spawn_raw(path: &[u8], priority: u8, flags: u16, actions: &[SpawnFdAction]) -> i32 {
    let attrs = SpawnAttrs {
        priority,
        _pad: [0; 3],
        flags,
        _pad2: 0,
        actions_ptr: actions.as_ptr() as u64,
        actions_len: actions.len() as u64,
        sigdefault_mask: 0,
    };
    let argv: [*const u8; 0] = [];
    unsafe {
        syscall5(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            argv.as_ptr() as u64,
            0,
            &attrs as *const SpawnAttrs as u64,
        ) as i32
    }
}

fn expect(label: &str, got: i32, want: i32) -> bool {
    if got == want {
        return true;
    }
    eprintln!("spawn_privilege_test: {label} returned {got}, want {want}");
    false
}

/// Privileged bits are refused with `EPERM` — the caller is asking for
/// something real that it may not have, not making a malformed request.
///
/// `SYSTEM` is refused even though this caller holds it.
fn privileged_flags_are_eperm() -> bool {
    expect(
        "COMPOSITOR",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_COMPOSITOR, &[]),
        EPERM,
    ) && expect(
        "DISPLAY_EXCLUSIVE",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_DISPLAY_EXCLUSIVE, &[]),
        EPERM,
    ) && expect(
        "SYSTEM (from a caller that holds SYSTEM)",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_SYSTEM, &[]),
        EPERM,
    ) && expect(
        "NO_PREEMPT",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_NO_PREEMPT, &[]),
        EPERM,
    )
}

/// Undefined bits fail closed so the ABI can grow one without a deployed caller
/// having already assigned it a different meaning. `0x0040` is the retired
/// `TASK_FLAG_FPU_INITIALIZED`: retired, not freed.
///
/// `KERNEL_MODE` is diagnosed here rather than mislabelled `NoMem` downstream,
/// and a request carrying both a reserved and a privileged bit is answered as
/// malformed — so probing reserved bits cannot learn from an `EPERM` that a bit
/// means something.
fn malformed_flags_are_einval() -> bool {
    expect(
        "undefined bit 0x0200",
        spawn_raw(MISSING, PRIORITY_NORMAL, 0x0200, &[]),
        EINVAL,
    ) && expect(
        "retired FPU_INITIALIZED bit 0x0040",
        spawn_raw(MISSING, PRIORITY_NORMAL, 0x0040, &[]),
        EINVAL,
    ) && expect(
        "KERNEL_MODE",
        spawn_raw(MISSING, PRIORITY_NORMAL, TASK_FLAG_KERNEL_MODE, &[]),
        EINVAL,
    ) && expect(
        "reserved bit alongside a privileged one",
        spawn_raw(MISSING, PRIORITY_NORMAL, 0x0200 | TASK_FLAG_COMPOSITOR, &[]),
        EINVAL,
    )
}

/// User space picks between `Normal` and `Low` and nothing else. `High` is the
/// compositor's tier, granted by program identity; `KernelIo` is for kthreads;
/// `Idle` is the per-CPU idle loop's.
fn privileged_tiers_are_einval() -> bool {
    expect(
        "High",
        spawn_raw(MISSING, PRIORITY_HIGH, TASK_FLAG_USER_MODE, &[]),
        EINVAL,
    ) && expect(
        "KernelIo",
        spawn_raw(MISSING, PRIORITY_KERNEL_IO, TASK_FLAG_USER_MODE, &[]),
        EINVAL,
    ) && expect(
        "Idle",
        spawn_raw(MISSING, PRIORITY_IDLE, TASK_FLAG_USER_MODE, &[]),
        EINVAL,
    )
}

/// The control: legal attrs clear validation and reach exec, which reports the
/// real load error. Without this every case above would also pass against a
/// handler that refused everything.
fn user_settable_flags_reach_exec() -> bool {
    expect(
        "USER_MODE|NEW_PGRP on a missing binary",
        spawn_raw(
            MISSING,
            PRIORITY_NORMAL,
            TASK_FLAG_USER_MODE | TASK_FLAG_NEW_PGRP,
            &[],
        ),
        ENOENT,
    )
}

/// The other half of the control: a legal spawn of a real binary still starts a
/// child and still reaps.
///
/// `TASK_FLAG_FOREGROUND` is deliberately absent — it would move the harness
/// console's foreground group to the child.
fn ordinary_spawn_still_works() -> bool {
    let stdio = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = spawn_raw(
        b"/bin/ifconfig",
        PRIORITY_NORMAL,
        TASK_FLAG_USER_MODE,
        &stdio,
    );
    if tid <= 0 {
        eprintln!("spawn_privilege_test: ordinary spawn of /bin/ifconfig returned {tid}");
        return false;
    }
    process::waitpid(tid as u32);
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("privileged_flags_are_eperm", privileged_flags_are_eperm),
    ("malformed_flags_are_einval", malformed_flags_are_einval),
    ("privileged_tiers_are_einval", privileged_tiers_are_einval),
    (
        "user_settable_flags_reach_exec",
        user_settable_flags_reach_exec,
    ),
    ("ordinary_spawn_still_works", ordinary_spawn_still_works),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
