#![feature(restricted_std)]

use slopos_userland::apps::shell;

fn main() {
    eprintln!("io_capture_test: start");

    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    // Test 1: run `ifconfig` (known working registry-spawn program)
    eprintln!("io_capture_test: running ifconfig...");
    static TOK_IFCONFIG: &[u8] = b"ifconfig\0";
    let argv1 = [TOK_IFCONFIG.as_ptr()];
    let rc1 = shell::exec::execute_tokens(argv1.len() as i32, &argv1);
    eprintln!("io_capture_test: ifconfig exit={}", rc1);

    // Test 2: run `nc -h` (registry-spawn, prints usage, exits immediately)
    eprintln!("io_capture_test: running nc -h...");
    static TOK_NC: &[u8] = b"nc\0";
    static TOK_H: &[u8] = b"-h\0";
    let argv2 = [TOK_NC.as_ptr(), TOK_H.as_ptr()];
    let rc2 = shell::exec::execute_tokens(argv2.len() as i32, &argv2);
    eprintln!("io_capture_test: nc -h exit={}", rc2);

    // Test 3: run `nc` with no args (should show usage, exit 1)
    eprintln!("io_capture_test: running nc (no args)...");
    let argv3 = [TOK_NC.as_ptr()];
    let rc3 = shell::exec::execute_tokens(argv3.len() as i32, &argv3);
    eprintln!("io_capture_test: nc exit={}", rc3);

    if rc2 != 0 {
        eprintln!("io_capture_test: FAIL nc -h returned {}, expected 0", rc2);
        std::process::exit(1);
    }
    if rc3 != 1 {
        eprintln!(
            "io_capture_test: FAIL nc (no args) returned {}, expected 1",
            rc3
        );
        std::process::exit(1);
    }

    eprintln!("io_capture_test: PASS");
    std::process::exit(0);
}
