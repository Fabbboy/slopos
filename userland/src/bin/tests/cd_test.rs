#![feature(restricted_std)]

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use std::env;
use std::fs;

/// Regression for the "`cd` always says not found" bug.
///
/// `cd` is `std::env::set_current_dir`, and it used to *always* fail because
/// std's `sys::paths` module had no SlopOS arm and fell back to the
/// `Unsupported` stub — even though the kernel chdir/getcwd syscalls work and
/// `ls` (which uses `std::fs`) succeeds. This walks every directory `ls /`
/// reports and `cd`s into each via plain `std::env`, proving third-party
/// userland apps can rely on `std` for cwd ops.
fn std_cd_into_every_listed_dir() -> bool {
    if env::set_current_dir("/").is_err() {
        return false;
    }

    let rd = match fs::read_dir("/") {
        Ok(rd) => rd,
        Err(_) => return false,
    };

    let mut dirs = 0u32;
    for entry in rd.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();

        // `cmd_cd` stats the target first…
        if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
            return false;
        }
        // …then `set_current_dir` into it (this is what used to always fail).
        if env::set_current_dir(&path).is_err() {
            return false;
        }
        dirs += 1;
        if env::set_current_dir("/").is_err() {
            return false;
        }
    }

    dirs > 0
}

/// `set_current_dir` then `current_dir` round-trips through getcwd/chdir.
fn std_set_then_current_dir_roundtrip() -> bool {
    if !fs::metadata("/bin").map(|m| m.is_dir()).unwrap_or(false) {
        // No /bin (e.g. ramfs root): assert root round-trip instead.
        if env::set_current_dir("/").is_err() {
            return false;
        }
        return env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s == "/"))
            .unwrap_or(false);
    }

    if env::set_current_dir("/bin").is_err() {
        return false;
    }
    let ok = env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s == "/bin"))
        .unwrap_or(false);
    let _ = env::set_current_dir("/");
    ok
}

/// `std::env::temp_dir()` resolves (wired to `/tmp`).
fn std_temp_dir_is_tmp() -> bool {
    env::temp_dir().to_str() == Some("/tmp")
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("std_cd_into_every_listed_dir", std_cd_into_every_listed_dir),
        (
            "std_set_then_current_dir_roundtrip",
            std_set_then_current_dir_roundtrip,
        ),
        ("std_temp_dir_is_tmp", std_temp_dir_is_tmp),
    ]);
}
