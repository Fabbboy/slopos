#![feature(restricted_std)]

use slopos_userland as _;

use std::env;
use std::fs;

/// Walks every directory `/` reports and `cd`s into each through plain
/// `std::env`, proving third-party userland apps can rely on `std` for cwd ops.
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

        if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
            return false;
        }
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

fn std_set_then_current_dir_roundtrip() -> bool {
    if !fs::metadata("/bin").map(|m| m.is_dir()).unwrap_or(false) {
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
