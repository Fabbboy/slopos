use slopos_abi::signal::{SIGCONT, SIGINT, SIGKILL};

use crate::runtime;
use crate::syscall::{UserSysInfo, core as sys_core, process};

use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::exec;
use super::super::jobs;

fn parse_job_id(ptr: *const u8) -> Option<u16> {
    if ptr.is_null() {
        return None;
    }
    let len = runtime::u_strlen(ptr);
    if len < 2 {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    if bytes[0] != b'%' {
        return None;
    }
    let mut id: u16 = 0;
    for &b in &bytes[1..] {
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

pub fn cmd_jobs(_argc: i32, _argv: &[*const u8]) -> i32 {
    jobs::refresh_liveness();
    jobs::render_jobs();
    0
}

pub fn cmd_kill(argc: i32, argv: &[*const u8]) -> i32 {
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

pub fn cmd_fg(argc: i32, argv: &[*const u8]) -> i32 {
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

pub fn cmd_bg(argc: i32, argv: &[*const u8]) -> i32 {
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

pub fn cmd_wait(argc: i32, argv: &[*const u8]) -> i32 {
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

pub fn cmd_exec(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"exec: missing path\n", COLOR_ERROR_RED);
        return 1;
    }

    let path_ptr = argv[1];
    if path_ptr.is_null() {
        shell_write_idx(b"exec: invalid path\n", COLOR_ERROR_RED);
        return 1;
    }

    let rc = process::exec_ptr(path_ptr);
    if rc < 0 {
        shell_write_idx(b"exec: failed\n", COLOR_ERROR_RED);
        1
    } else {
        0
    }
}

pub fn cmd_ps(_argc: i32, _argv: &[*const u8]) -> i32 {
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

pub fn maybe_handle_ctrl_c() -> bool {
    let fg = exec::foreground_pgid();
    if fg == 0 {
        return false;
    }
    if let Ok(group) = i32::try_from(fg) {
        let _ = process::kill_pid(-group, SIGINT);
    }
    true
}
