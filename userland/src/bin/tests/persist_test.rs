#![feature(restricted_std)]

use slopos_userland as _;

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};

// `/` is a RAM root under `root=auto`; only the ext2 mount can persist.
const PROBE: &str = "/mnt/persist-probe";
const PAYLOAD: &[u8] = b"slopos-persist-v1\n";

/// Self-detecting, so one ISO serves both boots of `just test-persist`.
fn persist_roundtrip() -> bool {
    match File::open(PROBE) {
        Ok(mut f) => {
            let mut got = Vec::new();
            if f.read_to_end(&mut got).is_err() {
                println!("PERSIST: probe unreadable");
                return false;
            }
            if got == PAYLOAD {
                println!("PERSIST: verified");
                true
            } else {
                println!("PERSIST: payload mismatch ({} bytes)", got.len());
                false
            }
        }
        // Only an absent probe means "first boot"; seeding over any other
        // error would report the failure this test exists to catch as a pass.
        Err(e) if e.kind() != ErrorKind::NotFound => {
            println!("PERSIST: probe unopenable: {e}");
            false
        }
        Err(_) => {
            let mut f = match File::create(PROBE) {
                Ok(f) => f,
                Err(e) => {
                    println!("PERSIST: create failed: {e}");
                    return false;
                }
            };
            if f.write_all(PAYLOAD).is_err() {
                println!("PERSIST: write failed");
                return false;
            }
            // Without this the payload sits in the block cache.
            if let Err(e) = f.sync_all() {
                println!("PERSIST: sync_all failed: {e}");
                return false;
            }
            drop(f);

            match fs::read(PROBE) {
                Ok(got) if got == PAYLOAD => {
                    println!("PERSIST: seeded");
                    true
                }
                _ => {
                    println!("PERSIST: seeded probe did not read back");
                    false
                }
            }
        }
    }
}

fn sync_all_mounts() -> bool {
    slopos_userland::syscall::fs::sync().is_ok()
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("persist_roundtrip", persist_roundtrip),
        ("sync_all_mounts", sync_all_mounts),
    ]);
}
