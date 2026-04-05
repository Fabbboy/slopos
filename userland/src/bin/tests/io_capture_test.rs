#![feature(restricted_std)]

use slopos_userland::apps::shell;

fn main() {
    eprintln!("io_capture_test: start");

    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    // Test 1: run `ifconfig` (known working registry-spawn program)
    eprintln!("io_capture_test: running ifconfig...");
    let mut tokens1 = shell::buffers::ParsedTokens::new();
    tokens1.push_token(b"ifconfig");
    let rc1 = shell::exec::execute_tokens(&tokens1);
    eprintln!("io_capture_test: ifconfig exit={}", rc1);

    // Test 2: run `nc -h` (registry-spawn, prints usage, exits immediately)
    eprintln!("io_capture_test: running nc -h...");
    let mut tokens2 = shell::buffers::ParsedTokens::new();
    tokens2.push_token(b"nc");
    tokens2.push_token(b"-h");
    let rc2 = shell::exec::execute_tokens(&tokens2);
    eprintln!("io_capture_test: nc -h exit={}", rc2);

    // Test 3: run `nc` with no args (should show usage, exit 1)
    eprintln!("io_capture_test: running nc (no args)...");
    let mut tokens3 = shell::buffers::ParsedTokens::new();
    tokens3.push_token(b"nc");
    let rc3 = shell::exec::execute_tokens(&tokens3);
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
