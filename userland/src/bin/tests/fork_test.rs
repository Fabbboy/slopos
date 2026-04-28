#![feature(restricted_std)]

use std::fs;

use slopos_userland::apps::shell;

const EXPECTED: &[u8] = b"piped text\n";

/// Single integration subtest: build an `echo … | tee` pipeline through
/// the shell's exec path and verify the file contents. Exit-code
/// distinctions from the legacy harness (20/22/23) collapse to a single
/// pass/fail bool; the kernel-side runner attaches one indented KTAP
/// subtest line for this case.
fn test_fork_pipe_echo_tee() -> bool {
    eprintln!("fork_test: pipeline repro start");

    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    let _ = fs::remove_file("/tmp/tee.txt");

    let mut tokens = shell::buffers::ParsedTokens::new();
    tokens.push_token(b"echo");
    tokens.push_token(b"piped text");
    tokens.push_token(b"|");
    tokens.push_token(b"tee");
    tokens.push_token(b"/tmp/tee.txt");

    let rc = shell::exec::execute_tokens(&tokens);
    if rc != 0 {
        eprintln!("fork_test: execute_tokens failed (rc={rc})");
        return false;
    }

    let out = match fs::read("/tmp/tee.txt") {
        Ok(out) => out,
        Err(e) => {
            eprintln!("fork_test: verify read failed: {e:?}");
            return false;
        }
    };

    if out.as_slice() != EXPECTED {
        eprintln!("fork_test: verify mismatch");
        return false;
    }

    eprintln!("fork_test: pipeline repro PASS");
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[("fork_pipe_echo_tee", test_fork_pipe_echo_tee)]);
}
