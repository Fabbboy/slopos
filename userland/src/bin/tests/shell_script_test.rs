#![feature(restricted_std)]

//! Non-interactive shell regression tests.
//!
//! `yes "curl http://google.com" | shell` used to reprint the prompt for every
//! command and then run truncated fragments of the line: the shell had no
//! non-interactive mode, so a raw-mode line editor ran on a pipe, decoded a
//! fixed 256-byte read into a queue, and dropped whatever was left of that
//! queue when the line ended.  With a 23-byte command line and `256 % 23 == 3`
//! the stream advanced three bytes per command, which is exactly the sequence
//! of fragments the report showed.
//!
//! Each case here feeds `/bin/shell` a script down a pipe and asserts on the
//! exact bytes it produces.  Two properties do the work: the output must be the
//! script's output and nothing else — no banner, no prompt, no SGR — and every
//! line must run exactly once, which a reader that over-reads cannot manage.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point is
// linked into the binary (same requirement as the sibling test bins; without
// it the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::apps::shell::script::SCRIPT_LINE_MAX;
use slopos_userland::syscall::{SyscallError, core as sys_core, fs, process};

/// Bounded wait so a regressed shell FAILS the case rather than hanging the
/// whole harness.
const REAP_SPINS: usize = 5000;

/// Run `script` through `/bin/shell` and collect its stdout and exit status.
///
/// Feeding and draining are interleaved on non-blocking descriptors: a script
/// larger than one pipe buffer would otherwise block the parent in `write`
/// while the child blocks in `write` on an output pipe nobody is reading.
fn run_script(script: &[u8]) -> Option<(Vec<u8>, i32)> {
    let (script_r, script_w) = fs::pipe().ok()?;
    let (out_r, out_w) = fs::pipe().ok()?;
    let script_r = script_r.into_raw();
    let script_w = script_w.into_raw();
    let out_r = out_r.into_raw();
    let out_w = out_w.into_raw();

    // No TASK_FLAG_FOREGROUND / TASK_FLAG_NEW_PGRP: this test must not move the
    // harness console's foreground process group.
    let actions = [
        process::clone_fd(script_r, 0),
        process::clone_fd(out_w, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_path_with_actions(
        b"/bin/shell",
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );

    // The child holds the only copy of each end it reads from or writes to, so
    // it sees EOF on its script and the parent sees EOF on its output.
    let _ = fs::close_fd_raw(script_r);
    let _ = fs::close_fd_raw(out_w);

    if tid <= 0 {
        eprintln!("shell_script_test: spawn of /bin/shell returned {tid}");
        let _ = fs::close_fd_raw(script_w);
        let _ = fs::close_fd_raw(out_r);
        return None;
    }

    let _ = fs::set_fd_nonblocking(script_w);
    let _ = fs::set_fd_nonblocking(out_r);

    let mut written = 0usize;
    let mut script_open = true;
    let mut output = Vec::new();
    let mut buf = [0u8; 512];
    let mut idle = 0usize;

    loop {
        let mut progress = false;

        if script_open {
            match fs::write_slice(script_w, &script[written..]) {
                Ok(n) if n > 0 => {
                    written += n;
                    progress = true;
                }
                Err(SyscallError::EAGAIN) => {}
                _ => written = script.len(),
            }
            if written >= script.len() {
                // Closing the write end is what gives the shell its EOF.
                let _ = fs::close_fd_raw(script_w);
                script_open = false;
                progress = true;
            }
        }

        match fs::read_slice(out_r, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                progress = true;
            }
            Err(SyscallError::EAGAIN) => {}
            Err(_) => break,
        }

        if progress {
            idle = 0;
        } else {
            idle += 1;
            if idle > REAP_SPINS {
                eprintln!("shell_script_test: no progress draining /bin/shell");
                break;
            }
            sys_core::sleep_ms(1);
        }
    }
    if script_open {
        let _ = fs::close_fd_raw(script_w);
    }
    let _ = fs::close_fd_raw(out_r);

    let pid = tid as u32;
    for _ in 0..REAP_SPINS {
        if let Some(status) = process::waitpid_nohang(pid) {
            return Some((output, status));
        }
        sys_core::sleep_ms(1);
    }
    eprintln!("shell_script_test: /bin/shell never exited");
    None
}

fn expect_output(name: &str, script: &[u8], want: &[u8]) -> bool {
    let Some((got, _)) = run_script(script) else {
        return false;
    };
    if got != want {
        eprintln!(
            "shell_script_test: {name}: output mismatch\n  want: {:?}\n  got:  {:?}",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(&got)
        );
        return false;
    }
    true
}

fn expect_status(name: &str, script: &[u8], want: i32) -> bool {
    let Some((_, status)) = run_script(script) else {
        return false;
    };
    if status != want {
        eprintln!("shell_script_test: {name}: status {status}, want {want}");
        return false;
    }
    true
}

/// The headline property: a script's stdout is the script's output and nothing
/// else.  Any banner, prompt, SGR run or bracketed-paste toggle fails this.
fn script_output_is_exact() -> bool {
    expect_output(
        "script_output_is_exact",
        b"echo one\necho two\necho three\n",
        b"one\ntwo\nthree\n",
    )
}

/// Forty short lines span several 256-byte reads, so a reader that keeps a
/// fixed chunk and discards the rest loses most of them.  Every line must
/// appear exactly once and in order.
fn every_line_runs_once_in_order() -> bool {
    let mut script = Vec::new();
    let mut want = Vec::new();
    for i in 0..40u32 {
        script.extend_from_slice(b"echo L");
        want.extend_from_slice(b"L");
        for part in [i / 10, i % 10] {
            script.push(b'0' + part as u8);
            want.push(b'0' + part as u8);
        }
        script.push(b'\n');
        want.push(b'\n');
    }
    expect_output("every_line_runs_once_in_order", &script, &want)
}

/// The regression test for the reported bug, stated as the property it broke:
/// the shell shares its script descriptor with the commands it runs, so it must
/// consume exactly the line it is about to execute.  `cat` here reads the
/// remainder of the script — which is only there if the shell left it.
fn no_overread_leaves_stdin_for_the_child() -> bool {
    expect_output(
        "no_overread_leaves_stdin_for_the_child",
        b"cat\npayload-a\npayload-b\n",
        b"payload-a\npayload-b\n",
    )
}

/// A script exits with the status of its last command.
fn exit_status_is_last_command() -> bool {
    expect_status("exit_status_is_last_command/false", b"true\nfalse\n", 1)
        && expect_status("exit_status_is_last_command/true", b"false\ntrue\n", 0)
}

/// Diagnostics belong on stderr; stdout stays the program's data.  `sh: NAME:
/// not found` used to be written to stdout, so it landed in the pipeline.
fn diagnostics_go_to_stderr() -> bool {
    expect_output(
        "diagnostics_go_to_stderr",
        b"echo before\nnosuchcmd\necho after\n",
        b"before\nafter\n",
    ) && expect_status("diagnostics_go_to_stderr/status", b"nosuchcmd\n", 127)
}

/// An over-long line is refused, not truncated: running a shortened command
/// means running a different command from the one that was written.
fn over_long_line_is_diagnosed_not_truncated() -> bool {
    let mut script = Vec::new();
    script.extend_from_slice(b"echo ");
    script.resize(script.len() + SCRIPT_LINE_MAX + 64, b'x');
    script.push(b'\n');
    script.extend_from_slice(b"echo after\n");
    // The over-long line produces nothing, and — the part that matters — the
    // line after it is a whole line rather than the tail of the one refused.
    expect_output(
        "over_long_line_is_diagnosed_not_truncated",
        &script,
        b"after\n",
    )
}

fn comments_are_ignored() -> bool {
    expect_output(
        "comments_are_ignored",
        b"# a comment\necho ok   # trailing\necho a#b\n",
        b"ok\na#b\n",
    )
}

fn crlf_script_lines() -> bool {
    expect_output(
        "crlf_script_lines",
        b"echo one\r\necho two\r\n",
        b"one\ntwo\n",
    )
}

fn blank_lines_are_skipped() -> bool {
    expect_output("blank_lines_are_skipped", b"\n\n   \necho ok\n", b"ok\n")
}

/// `exit` did not exist at all; the shell could not be ended from a script.
fn exit_builtin_terminates_with_status() -> bool {
    expect_output(
        "exit_builtin_terminates_with_status",
        b"echo a\nexit 3\necho b\n",
        b"a\n",
    ) && expect_status(
        "exit_builtin_terminates_with_status",
        b"echo a\nexit 3\necho b\n",
        3,
    )
}

/// `echo hi; echo bye` used to print the literal string `hi; echo bye` and
/// report success — a wrong result with no error.
fn sequence_and_shortcircuit() -> bool {
    expect_output(
        "sequence_and_shortcircuit",
        b"echo hi; echo bye\nfalse && echo no\nfalse || echo yes\ntrue && echo also\n",
        b"hi\nbye\nyes\nalso\n",
    )
}

/// `cmd 2>&1` used to run `cmd 2 1` with stdout truncated into a file named `&`.
fn stderr_redirection() -> bool {
    expect_output(
        "stderr_redirection",
        b"nosuchcmd 2>/dev/null\necho ok\n",
        b"ok\n",
    )
}

/// An assignment prefix applies to one command and is put back afterwards; a
/// bare assignment sets a shell variable.
fn assignments_scope_correctly() -> bool {
    expect_output(
        "assignments_scope_correctly",
        b"FOO=bar\necho $FOO\nFOO=baz echo $FOO\n",
        b"bar\nbar\n",
    )
}

/// `sh -c` did not exist: `main()` ignored argv entirely.
fn dash_c_runs_the_string() -> bool {
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    // argv[0] is the program name, as everywhere else: the shell's option
    // parsing skips it, exactly as `std::env::args()` presents it.
    let arg0 = *b"shell\0";
    let arg_c = *b"-c\0";
    let arg_cmd = *b"exit 7\0";
    let argv = [arg0.as_ptr(), arg_c.as_ptr(), arg_cmd.as_ptr()];
    let tid = process::spawn_path_with_actions(
        b"/bin/shell",
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        eprintln!("shell_script_test: dash_c spawn returned {tid}");
        return false;
    }
    let status = process::waitpid(tid as u32);
    if status != 7 {
        eprintln!("shell_script_test: `shell -c 'exit 7'` status {status}, want 7");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("script_output_is_exact", script_output_is_exact),
    (
        "every_line_runs_once_in_order",
        every_line_runs_once_in_order,
    ),
    (
        "no_overread_leaves_stdin_for_the_child",
        no_overread_leaves_stdin_for_the_child,
    ),
    ("exit_status_is_last_command", exit_status_is_last_command),
    ("diagnostics_go_to_stderr", diagnostics_go_to_stderr),
    (
        "over_long_line_is_diagnosed_not_truncated",
        over_long_line_is_diagnosed_not_truncated,
    ),
    ("comments_are_ignored", comments_are_ignored),
    ("crlf_script_lines", crlf_script_lines),
    ("blank_lines_are_skipped", blank_lines_are_skipped),
    (
        "exit_builtin_terminates_with_status",
        exit_builtin_terminates_with_status,
    ),
    ("sequence_and_shortcircuit", sequence_and_shortcircuit),
    ("stderr_redirection", stderr_redirection),
    ("assignments_scope_correctly", assignments_scope_correctly),
    ("dash_c_runs_the_string", dash_c_runs_the_string),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
