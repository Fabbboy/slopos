#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use std::fs;

use slopos_userland::keymap::{current_name as current, load_layout_by_name};
use slopos_userland::syscall::keymap::{keymap_get_name, keymap_load};

/// The same file → parse → serialise → upload pipeline the `keymap` builtin and
/// the init boot applier run.
fn load_layout_file(name: &str) -> bool {
    load_layout_by_name(name).is_ok()
}

/// `SYSCALL_KEYMAP_GET_NAME` returns the active layout name through the
/// SMAP-safe copy path; the boot default is `us`.
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

/// Full `keymap <name>` switch, then restore. Runs with `TASK_FLAG_SYSTEM` like
/// every utest binary, so it says nothing about the unprivileged-caller gate.
fn load_switches_layout_and_restores() -> bool {
    let de_ok = load_layout_file("de_CH") && current().as_deref() == Some("de_CH");
    // Restore US whatever happened above, so other tests are unaffected.
    let us_ok = load_layout_file("us") && current().as_deref() == Some("us");
    de_ok && us_ok
}

/// A malformed (wrong-size) blob is rejected with `EINVAL`, never installed.
fn load_rejects_bad_blob() -> bool {
    keymap_load(&[0u8; 16]) < 0
}

/// The `keymap <unknown>` path: a missing file must return `Err`, not panic.
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
