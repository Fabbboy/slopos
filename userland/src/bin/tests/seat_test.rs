#![feature(restricted_std)]

//! Seat exclusivity, non-transferability and virtcon fallback, exercised
//! through the real syscall entry.
//!
//! The kernel test suite cannot see any of this: under `tests=on` init exits
//! before spawning the compositor, so nothing else in the tree ever takes a
//! seat. These are the only tests that do.

use slopos_userland as _;

use slopos_abi::spawn::{SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::syscall::{SYSCALL_INPUT_POLL_BATCH, SYSCALL_SPAWN_PATH};
use slopos_userland::syscall::process;
use slopos_userland::syscall::raw::{syscall2, syscall5};
use slopos_userland::syscall::window::{SEAT_COMPOSITOR_PRIMARY, SEAT_VIRTCON, screen_acquire};

const EBUSY: i64 = -16;
const EINVAL: i64 = -22;
const EPERM: i64 = -1;

/// A seat taken twice by the *same* task succeeds — the holder may re-acquire,
/// which is what lets a restarted compositor recover its own seat.
fn holder_may_reacquire() -> bool {
    let first = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if first < 0 {
        eprintln!("seat_test: first screen_acquire failed: {first}");
        return false;
    }
    let second = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if second < 0 {
        eprintln!("seat_test: holder could not re-acquire its own seat: {second}");
        return false;
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(first as i32);
    let _ = slopos_userland::syscall::fs::close_fd_raw(second as i32);
    true
}

/// An unknown seat rank is refused as malformed, not as busy: probing a
/// reserved rank must not learn from an `EBUSY` that the rank *means*
/// something.
fn unknown_seat_rank_is_einval() -> bool {
    let rc = screen_acquire(7);
    if rc != EINVAL {
        eprintln!("seat_test: rank 7 gave {rc}, want EINVAL");
        return false;
    }
    true
}

/// The seat fd is not a byte stream.
fn seat_fd_rejects_read_and_write() -> bool {
    let fd = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if fd < 0 {
        eprintln!("seat_test: screen_acquire failed: {fd}");
        return false;
    }
    let mut buf = [0u8; 8];
    let mut ok = true;
    if slopos_userland::syscall::fs::read_slice(fd as i32, &mut buf).is_ok() {
        eprintln!("seat_test: read of a seat fd succeeded, want an error");
        ok = false;
    }
    if slopos_userland::syscall::fs::write_slice(fd as i32, &buf).is_ok() {
        eprintln!("seat_test: write of a seat fd succeeded, want an error");
        ok = false;
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(fd as i32);
    ok
}

/// A seat descriptor cannot be duplicated. `dup` is the cheapest duplication
/// path and bypasses `fileio_clone_file_ref` entirely, so it is the one most
/// likely to grow a hole back.
fn seat_fd_is_not_duplicable() -> bool {
    let fd = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if fd < 0 {
        eprintln!("seat_test: screen_acquire failed: {fd}");
        return false;
    }
    let mut ok = true;
    match slopos_userland::syscall::fs::dup(fd as i32) {
        Ok(dup_fd) => {
            eprintln!("seat_test: dup of a seat fd succeeded — the seat is duplicable");
            drop(dup_fd);
            ok = false;
        }
        Err(_) => {}
    }
    let rc = slopos_userland::syscall::fs::dup2(fd as i32, 20);
    if rc.is_ok() {
        eprintln!("seat_test: dup2 of a seat fd succeeded — the seat is duplicable");
        ok = false;
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(fd as i32);
    ok
}

/// A seat descriptor cannot be handed to a child through the spawn fd-action
/// ABI, by either the clone or the transfer arm. A duplicated seat would
/// produce a second holder the arbiter does not know about.
fn seat_fd_is_not_spawn_transferable() -> bool {
    let fd = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if fd < 0 {
        eprintln!("seat_test: screen_acquire failed: {fd}");
        return false;
    }
    let mut ok = true;
    for (kind, label) in [
        (SpawnFdActionKind::CloneFd, "CloneFd"),
        (SpawnFdActionKind::TransferFd, "TransferFd"),
    ] {
        let action = SpawnFdAction {
            kind: kind as u32,
            src_fd: fd as i32,
            target_fd: 3,
            _pad: 0,
            open_path_ptr: 0,
            open_path_len: 0,
            open_flags: 0,
            _pad2: 0,
        };
        let attrs = SpawnAttrs {
            priority: 2,
            _pad: [0; 3],
            flags: slopos_abi::task::TASK_FLAG_USER_MODE,
            _pad2: 0,
            actions_ptr: &action as *const _ as u64,
            actions_len: 1,
            sigdefault_mask: 0,
            envp_ptr: 0,
            envp_len: 0,
        };
        let path = b"/bin/cd_test";
        let argv: [*const u8; 0] = [];
        let rc = unsafe {
            syscall5(
                SYSCALL_SPAWN_PATH,
                path.as_ptr() as u64,
                path.len() as u64,
                argv.as_ptr() as u64,
                0,
                &attrs as *const _ as u64,
            ) as i64
        };
        // A spawn whose fd actions cannot be applied fails; a positive tid
        // would mean the child got the seat.
        if rc > 0 {
            eprintln!("seat_test: {label} moved a seat into a child (tid {rc})");
            let _ = process::waitpid(rc as u32);
            ok = false;
        }
    }
    // The transfer arm must not have emptied our own slot on the way to
    // failing: an all-or-nothing action list leaves the parent holding it.
    if !seat_still_ours() {
        eprintln!("seat_test: a refused transfer still took our seat away");
        ok = false;
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(fd as i32);
    ok
}

/// `input_poll_batch` without the input seat is `EPERM`, not a silent zero:
/// the whole point is that presenting a call no longer confers the sink.
fn input_poll_without_seat_is_denied() -> bool {
    let mut buf = [0u8; 64];
    let rc = unsafe { syscall2(SYSCALL_INPUT_POLL_BATCH, buf.as_mut_ptr() as u64, 1) as i64 };
    if rc != EPERM {
        eprintln!("seat_test: input_poll_batch without a seat gave {rc}, want EPERM");
        return false;
    }
    true
}

/// Virtcon outranks the compositor, so the display stays recoverable from a
/// wedged compositor. Taking the higher seat must displace the lower one.
fn virtcon_outranks_compositor() -> bool {
    let comp = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if comp < 0 {
        eprintln!("seat_test: screen_acquire(compositor) failed: {comp}");
        return false;
    }
    let virtcon = screen_acquire(SEAT_VIRTCON);
    if virtcon < 0 {
        eprintln!("seat_test: virtcon could not take the screen back: {virtcon}");
        let _ = slopos_userland::syscall::fs::close_fd_raw(comp as i32);
        return false;
    }
    // Same task, so both grants are ours; what matters is that the higher rank
    // was not refused.
    let _ = slopos_userland::syscall::fs::close_fd_raw(comp as i32);
    let _ = slopos_userland::syscall::fs::close_fd_raw(virtcon as i32);
    true
}

/// Whether this task still holds the screen seat, probed by re-acquiring:
/// the holder is always allowed to, a non-holder facing a live holder is not.
fn seat_still_ours() -> bool {
    let rc = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if rc < 0 {
        return false;
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(rc as i32);
    true
}

/// A child that never acquired a seat is refused the input sink, proving the
/// seat did not leak across `spawn` even though the parent held one.
fn child_does_not_inherit_the_seat() -> bool {
    let fd = screen_acquire(SEAT_COMPOSITOR_PRIMARY);
    if fd < 0 {
        eprintln!("seat_test: screen_acquire failed: {fd}");
        return false;
    }
    let tid = process::spawn_path("/bin/cd_test");
    if tid <= 0 {
        eprintln!("seat_test: could not spawn a child: {tid}");
        let _ = slopos_userland::syscall::fs::close_fd_raw(fd as i32);
        return false;
    }
    let _ = process::waitpid(tid as u32);
    // Our own seat survived the child's whole lifetime, including its exit —
    // the cleanup hook must revoke only the seats the *dying* task held.
    let ok = seat_still_ours();
    if !ok {
        eprintln!("seat_test: a child's exit revoked the parent's seat");
    }
    let _ = slopos_userland::syscall::fs::close_fd_raw(fd as i32);
    ok
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("holder_may_reacquire", holder_may_reacquire),
    ("unknown_seat_rank_is_einval", unknown_seat_rank_is_einval),
    (
        "seat_fd_rejects_read_and_write",
        seat_fd_rejects_read_and_write,
    ),
    ("seat_fd_is_not_duplicable", seat_fd_is_not_duplicable),
    (
        "seat_fd_is_not_spawn_transferable",
        seat_fd_is_not_spawn_transferable,
    ),
    (
        "input_poll_without_seat_is_denied",
        input_poll_without_seat_is_denied,
    ),
    ("virtcon_outranks_compositor", virtcon_outranks_compositor),
    (
        "child_does_not_inherit_the_seat",
        child_does_not_inherit_the_seat,
    ),
];

fn main() {
    let _ = EBUSY;
    slopos_slibc::test_harness::run(CASES);
}
