#![feature(restricted_std)]

//! A spawned program's output must reach the process that spawned it.
//!
//! `exec` replaces the program image, and everything about where that image
//! lands has to agree: the code VMA the address space records, the base the
//! loader writes segments at, and the entry point the task resumes on. When
//! those disagreed the child was created, `exec` logged a successful load, and
//! the program then produced nothing — no output, no error, no fault. Nothing
//! in the kernel log said anything was wrong, which is what made it worth a
//! test rather than a look.

use slopos_userland::apps::shell;

use std::fs;

fn shell_ready() {
    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();
}

fn run(tokens: &[&[u8]]) -> i32 {
    let mut parsed = shell::buffers::ParsedTokens::new();
    for token in tokens {
        parsed.push_token(token);
    }
    shell::exec::execute_tokens(&parsed)
}

/// The load-bearing case: a spawned binary's stdout, redirected to a file, must
/// contain what the program printed. A child that never reached its entry point
/// exits cleanly and leaves the file empty.
fn spawned_program_output_is_not_empty() -> bool {
    shell_ready();
    let path = "/tmp/spawn_output.txt";
    let _ = fs::remove_file(path);

    let rc = run(&[
        b"echo",
        b"SPAWN_OUTPUT_MARKER",
        b">",
        b"/tmp/spawn_output.txt",
    ]);
    if rc != 0 {
        eprintln!("spawn_output: redirect pipeline returned {rc}");
        return false;
    }

    let out = match fs::read(path) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("spawn_output: reading {path} failed: {e:?}");
            return false;
        }
    };
    let _ = fs::remove_file(path);

    if out.is_empty() {
        eprintln!("spawn_output: the spawned program produced no output at all");
        return false;
    }
    if !out.windows(19).any(|w| w == b"SPAWN_OUTPUT_MARKER") {
        eprintln!(
            "spawn_output: output present but wrong: {:?}",
            &out[..out.len().min(64)]
        );
        return false;
    }
    true
}

/// A registry program spawned through the same path must reach its own `main`
/// and report an exit code it chose. `nc -h` prints usage and exits 0; a child
/// that never ran cannot produce that.
fn spawned_registry_program_reaches_main() -> bool {
    shell_ready();
    let rc = run(&[b"nc", b"-h"]);
    if rc != 0 {
        eprintln!("spawn_output: `nc -h` returned {rc}, expected 0");
        return false;
    }
    // The complement: the same binary with no arguments chooses a *different*
    // exit code. Both being right means the child ran its own logic rather
    // than a default the kernel supplied.
    let rc = run(&[b"nc"]);
    if rc != 1 {
        eprintln!("spawn_output: `nc` with no args returned {rc}, expected 1");
        return false;
    }
    true
}

/// Exec must survive being done twice in a row from one shell, which is the
/// ordinary case and the one that catches a reset leaving the address space
/// subtly wrong for the *next* image rather than this one.
fn consecutive_spawns_each_produce_output() -> bool {
    shell_ready();
    for round in 0..3 {
        let path = "/tmp/spawn_output_seq.txt";
        let _ = fs::remove_file(path);
        let rc = run(&[b"echo", b"ROUND", b">", b"/tmp/spawn_output_seq.txt"]);
        if rc != 0 {
            eprintln!("spawn_output: round {round} returned {rc}");
            return false;
        }
        match fs::read(path) {
            Ok(out) if !out.is_empty() => {}
            Ok(_) => {
                eprintln!("spawn_output: round {round} produced an empty file");
                return false;
            }
            Err(e) => {
                eprintln!("spawn_output: round {round} read failed: {e:?}");
                return false;
            }
        }
        let _ = fs::remove_file(path);
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "spawned_program_output_is_not_empty",
            spawned_program_output_is_not_empty,
        ),
        (
            "spawned_registry_program_reaches_main",
            spawned_registry_program_reaches_main,
        ),
        (
            "consecutive_spawns_each_produce_output",
            consecutive_spawns_each_produce_output,
        ),
    ]);
}
