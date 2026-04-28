#![feature(restricted_std)]

use slopos_userland::apps::shell;

fn ensure_shell_initialized() {
    shell::cwd_set(b"/");
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();
}

fn test_ifconfig() -> bool {
    ensure_shell_initialized();
    eprintln!("io_capture_test: running ifconfig...");
    let mut tokens = shell::buffers::ParsedTokens::new();
    tokens.push_token(b"ifconfig");
    let rc = shell::exec::execute_tokens(&tokens);
    eprintln!("io_capture_test: ifconfig exit={}", rc);
    // ifconfig is allowed to return any code; we just want to confirm it
    // can be spawned through the registry path without crashing.
    true
}

fn test_nc_help() -> bool {
    ensure_shell_initialized();
    eprintln!("io_capture_test: running nc -h...");
    let mut tokens = shell::buffers::ParsedTokens::new();
    tokens.push_token(b"nc");
    tokens.push_token(b"-h");
    let rc = shell::exec::execute_tokens(&tokens);
    eprintln!("io_capture_test: nc -h exit={}", rc);
    if rc != 0 {
        eprintln!("io_capture_test: FAIL nc -h returned {}, expected 0", rc);
        return false;
    }
    true
}

fn test_nc_no_args() -> bool {
    ensure_shell_initialized();
    eprintln!("io_capture_test: running nc (no args)...");
    let mut tokens = shell::buffers::ParsedTokens::new();
    tokens.push_token(b"nc");
    let rc = shell::exec::execute_tokens(&tokens);
    eprintln!("io_capture_test: nc exit={}", rc);
    if rc != 1 {
        eprintln!(
            "io_capture_test: FAIL nc (no args) returned {}, expected 1",
            rc
        );
        return false;
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("ifconfig", test_ifconfig),
        ("nc_help", test_nc_help),
        ("nc_no_args", test_nc_no_args),
    ]);
}
