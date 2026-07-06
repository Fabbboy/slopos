#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use std::fs;

use slopos_userland::keymap::{current_name as current, load_layout_by_name};
use slopos_userland::syscall::keymap::{keymap_get_name, keymap_load};

/// Read `/usr/share/keymaps/<name>.layout`, parse it in userland, serialise the
/// binary table, and upload it — the exact shared pipeline the `keymap` builtin
/// and the init boot applier run.
fn load_layout_file(name: &str) -> bool {
    load_layout_by_name(name).is_ok()
}

/// `keymap` with no args used to page-fault the kernel: `SYSCALL_KEYMAP_GET_NAME`
/// wrote the user buffer directly from supervisor mode, which SMAP forbids. This
/// exercises that exact syscall and asserts it returns the active layout name
/// (the boot default is `us`) — a regression guard for the SMAP-safe copy path.
fn get_name_returns_default() -> bool {
    current().as_deref() == Some("us")
}

/// The kernel honors the user buffer length: a 1-byte buffer gets exactly one
/// byte (`'u'`), never an overrun.
fn get_name_respects_buflen() -> bool {
    let mut one = [0u8; 1];
    let n = keymap_get_name(&mut one);
    n == 1 && one[0] == b'u'
}

/// Full end-to-end switch from an ordinary (non-console-admin) process: load
/// Swiss German, confirm the active layout changed, then restore US. Proves the
/// whole `keymap <name>` pipeline (file → parse → serialise → syscall → install)
/// works without elevated privilege.
fn load_switches_layout_and_restores() -> bool {
    let de_ok = load_layout_file("de_CH") && current().as_deref() == Some("de_CH");
    // Always restore US, whatever happened above, so the desktop/other tests are
    // unaffected.
    let us_ok = load_layout_file("us") && current().as_deref() == Some("us");
    de_ok && us_ok
}

/// A malformed (wrong-size) blob is rejected with `EINVAL` (negative), never
/// installed — the binary validator is the safety boundary.
fn load_rejects_bad_blob() -> bool {
    keymap_load(&[0u8; 16]) < 0
}

/// Reading a non-existent layout file must return `Err` cleanly (not panic) —
/// this is the `keymap <unknown>` path. If `std::fs::read` panics on a missing
/// file, this test (and the shell builtin) crash.
fn read_missing_returns_err() -> bool {
    fs::read("/usr/share/keymaps/__definitely_missing__.layout").is_err()
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("get_name_returns_default", get_name_returns_default),
        ("get_name_respects_buflen", get_name_respects_buflen),
        (
            "load_switches_layout_and_restores",
            load_switches_layout_and_restores,
        ),
        ("load_rejects_bad_blob", load_rejects_bad_blob),
        ("read_missing_returns_err", read_missing_returns_err),
    ]);
}
