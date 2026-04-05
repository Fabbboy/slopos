#![feature(restricted_std)]

use std::fs;

use slopos_userland::apps::shell;

const EXPECTED: &[u8] = b"piped text\n";

fn fail(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

fn main() {
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
        fail("fork_test: execute_tokens failed", 20);
    }

    let out = match fs::read("/tmp/tee.txt") {
        Ok(out) => out,
        Err(_) => fail("fork_test: verify read failed", 22),
    };

    if out.as_slice() != EXPECTED {
        fail("fork_test: verify mismatch", 23);
    }

    eprintln!("fork_test: pipeline repro PASS");
    std::process::exit(0);
}
