#![feature(restricted_std)]

use std::fs;

use slopos_userland::apps::shell;
use slopos_userland::syscall::{core as sys_core, tty};

const EXPECTED: &[u8] = b"piped text\n";

fn fail(msg: &[u8], code: i32) -> ! {
    let _ = tty::write(msg);
    sys_core::exit_with_code(code);
}

fn main() {
    let _ = tty::write(b"fork_test: pipeline repro start\n");

    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    let _ = fs::remove_file("/tmp/tee.txt");

    static TOK_ECHO: &[u8] = b"echo\0";
    static TOK_TEXT: &[u8] = b"piped text\0";
    static TOK_PIPE: &[u8] = b"|\0";
    static TOK_TEE: &[u8] = b"tee\0";
    static TOK_PATH: &[u8] = b"/tmp/tee.txt\0";

    let argv = [
        TOK_ECHO.as_ptr(),
        TOK_TEXT.as_ptr(),
        TOK_PIPE.as_ptr(),
        TOK_TEE.as_ptr(),
        TOK_PATH.as_ptr(),
    ];
    let rc = shell::exec::execute_tokens(argv.len() as i32, &argv);
    if rc != 0 {
        fail(b"fork_test: execute_tokens failed\n", 20);
    }

    let out = match fs::read("/tmp/tee.txt") {
        Ok(out) => out,
        Err(_) => fail(b"fork_test: verify read failed\n", 22),
    };

    if out.as_slice() != EXPECTED {
        fail(b"fork_test: verify mismatch\n", 23);
    }

    let _ = tty::write(b"fork_test: pipeline repro PASS\n");
    sys_core::exit_with_code(0);
}
