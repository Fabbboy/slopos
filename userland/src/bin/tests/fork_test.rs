//! Pipeline/fork regression test:
//! Exercises the actual shell pipeline path for:
//! `echo "piped text" | tee /tmp/tee.txt`

#![feature(restricted_std)]
#![no_main]

use core::ffi::c_char;

use slopos_abi::fs::USER_FS_OPEN_READ;
use slopos_userland::apps::shell;
use slopos_userland::syscall::{core as sys_core, fs, tty};

const TEST_PATH: &[u8] = b"/tmp/tee.txt\0";
const EXPECTED: &[u8] = b"piped text\n";

fn fail(msg: &[u8], code: i32) -> ! {
    let _ = tty::write(msg);
    sys_core::exit_with_code(code);
}

fn fork_test_main(_arg: *mut u8) {
    let _ = tty::write(b"fork_test: pipeline repro start\n");

    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    let _ = fs::unlink_path(TEST_PATH.as_ptr() as *const c_char);

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

    // Validate output file content.
    let in_fd = match fs::open_path(TEST_PATH.as_ptr() as *const c_char, USER_FS_OPEN_READ) {
        Ok(fd) => fd,
        Err(_) => fail(b"fork_test: verify open failed\n", 21),
    };
    let mut out = [0u8; 64];
    let read_len = match fs::read_slice(in_fd, &mut out) {
        Ok(n) => n,
        Err(_) => {
            let _ = fs::close_fd(in_fd);
            fail(b"fork_test: verify read failed\n", 22);
        }
    };
    let _ = fs::close_fd(in_fd);

    if read_len != EXPECTED.len() || &out[..read_len] != EXPECTED {
        fail(b"fork_test: verify mismatch\n", 23);
    }

    let _ = tty::write(b"fork_test: pipeline repro PASS\n");
    sys_core::exit_with_code(0);
}

slopos_userland::entry!(fork_test_main);
