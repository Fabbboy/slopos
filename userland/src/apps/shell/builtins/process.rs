use slopos_abi::signal::{SIGCONT, SIGKILL};

use crate::syscall::{UserSysInfo, core as sys_core, process};

use super::super::display::{COLOR_ERROR_RED, shell_error_named, shell_write, shell_write_idx};
use super::super::exec;
use super::super::jobs;

fn parse_job_id(arg: &[u8]) -> Option<u16> {
    if arg.len() < 2 {
        return None;
    }
    if arg[0] != b'%' {
        return None;
    }
    let mut id: u16 = 0;
    for &b in &arg[1..] {
        if !b.is_ascii_digit() {
            return None;
        }
        id = id.checked_mul(10)?;
        id = id.checked_add((b - b'0') as u16)?;
    }
    if id == 0 {
        return None;
    }
    Some(id)
}

pub fn cmd_jobs(_argc: i32, _argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    jobs::render_jobs();
    0
}

pub fn cmd_kill(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    if argc < 2 {
        shell_write_idx(b"kill: missing pid or %job\n", COLOR_ERROR_RED);
        return 1;
    }
    let target = argv[1];
    if let Some(job_id) = parse_job_id(target) {
        let Some(pgid) = jobs::find_pgid_by_job_id(job_id) else {
            shell_write_idx(b"kill: unknown job\n", COLOR_ERROR_RED);
            return 1;
        };
        if let Ok(group) = i32::try_from(pgid) {
            if process::kill_pid(-group, SIGKILL) < 0 {
                shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
                return 1;
            }
        } else {
            shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
            return 1;
        };
        let _ = jobs::remove_by_job_id(job_id);
        return 0;
    }
    let Some(pid) = jobs::parse_u32_arg(target) else {
        shell_write_idx(b"kill: invalid pid\n", COLOR_ERROR_RED);
        return 1;
    };
    if let Ok(target) = i32::try_from(pid) {
        if process::kill_pid(target, SIGKILL) < 0 {
            shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
            return 1;
        }
    } else {
        shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
        return 1;
    };
    let _ = jobs::remove_by_pid(pid);
    0
}

pub fn cmd_fg(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    if argc < 2 {
        shell_write_idx(b"fg: missing %job\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(job_id) = parse_job_id(argv[1]) else {
        shell_write_idx(b"fg: expected %job\n", COLOR_ERROR_RED);
        return 1;
    };
    let Some(pid) = jobs::find_pid_by_job_id(job_id) else {
        shell_write_idx(b"fg: unknown job\n", COLOR_ERROR_RED);
        return 1;
    };
    let Some(pgid) = jobs::find_pgid_by_job_id(job_id) else {
        shell_write_idx(b"fg: unknown job\n", COLOR_ERROR_RED);
        return 1;
    };

    if let Ok(group) = i32::try_from(pgid) {
        let _ = process::kill_pid(-group, SIGCONT);
    }
    exec::enter_foreground(pgid);
    let status = process::waitpid(pid);
    exec::leave_foreground();
    jobs::mark_done_by_pid(pid);
    let _ = jobs::remove_by_job_id(job_id);
    status
}

pub fn cmd_bg(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    if argc < 2 {
        shell_write_idx(b"bg: missing %job\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(job_id) = parse_job_id(argv[1]) else {
        shell_write_idx(b"bg: expected %job\n", COLOR_ERROR_RED);
        return 1;
    };
    let Some(pgid) = jobs::find_pgid_by_job_id(job_id) else {
        shell_write_idx(b"bg: unknown job\n", COLOR_ERROR_RED);
        return 1;
    };
    if let Ok(group) = i32::try_from(pgid) {
        if process::kill_pid(-group, SIGCONT) < 0 {
            shell_write_idx(b"bg: failed\n", COLOR_ERROR_RED);
            return 1;
        }
    } else {
        shell_write_idx(b"bg: failed\n", COLOR_ERROR_RED);
        return 1;
    };
    0
}

pub fn cmd_wait(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"wait: missing pid\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(pid) = jobs::parse_u32_arg(argv[1]) else {
        shell_write_idx(b"wait: invalid pid\n", COLOR_ERROR_RED);
        return 1;
    };
    process::waitpid(pid)
}

/// `exit [n]` — end the shell with status `n`, or with the status of the last
/// command when no operand is given.
///
/// In a forked pipeline stage the returned status *is* the exit, so `exit | true`
/// ends only the subshell. In the shell's own process the request is merely
/// recorded, leaving the command loop to restore redirects and hand back the
/// terminal.
pub fn cmd_exit(argc: i32, argv: &[&[u8]]) -> i32 {
    let status = if argc >= 2 {
        match jobs::parse_u32_arg(argv[1]) {
            Some(n) => (n & 0xFF) as i32,
            None => {
                shell_error_named(b"exit", b"numeric argument required");
                super::super::exec::STATUS_SYNTAX_ERROR
            }
        }
    } else {
        super::super::last_exit_code()
    };

    if !super::super::interrupt::in_forked_child() {
        super::super::request_exit(status);
    }
    status
}

pub fn cmd_exec(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"exec: missing path\n", COLOR_ERROR_RED);
        return 1;
    }

    let path = argv[1];
    if path.is_empty() {
        shell_write_idx(b"exec: invalid path\n", COLOR_ERROR_RED);
        return 1;
    }

    // `exec_ptr` takes a NUL-terminated pointer.
    let mut buf = [0u8; 256];
    let len = path.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&path[..len]);
    buf[len] = 0;

    let rc = process::exec_ptr(buf.as_ptr());
    if rc < 0 {
        shell_write_idx(b"exec: failed\n", COLOR_ERROR_RED);
        1
    } else {
        0
    }
}

pub fn cmd_ps(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"ps: failed\n", COLOR_ERROR_RED);
        return 1;
    }
    shell_write(b"tasks total: ");
    jobs::write_u64(info.total_tasks as u64);
    shell_write(b"\nactive: ");
    jobs::write_u64(info.active_tasks as u64);
    shell_write(b"\nready: ");
    jobs::write_u64(info.ready_tasks as u64);
    shell_write(b"\n");
    0
}
