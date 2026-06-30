#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use std::fs;

use slopos_keymap_core::{LayoutTable, SERIALIZED_LEN, parse, serialize};
use slopos_userland::syscall::keymap::{keymap_get_name, keymap_load};

/// The active layout name, or `None` on error.
fn current() -> Option<String> {
    let mut buf = [0u8; 16];
    let n = keymap_get_name(&mut buf);
    if n <= 0 {
        return None;
    }
    let n = (n as usize).min(buf.len());
    core::str::from_utf8(&buf[..n]).ok().map(String::from)
}

/// Read `/usr/share/keymaps/<name>.layout`, parse it in userland, serialise the
/// binary table, and upload it — the exact path the `keymap` builtin runs.
fn load_layout_file(name: &str) -> bool {
    let path = format!("/usr/share/keymaps/{name}.layout");
    let src = match fs::read(&path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut table = Box::new(LayoutTable::empty());
    if parse(&src, &mut table).is_err() {
        return false;
    }
    let mut blob = vec![0u8; SERIALIZED_LEN];
    if serialize(&table, &mut blob).is_err() {
        return false;
    }
    keymap_load(&blob) == 0
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
