use crate::program_registry;
use crate::syscall::{UserSysInfo, core as sys_core, process};

use super::super::display::{
    COLOR_COMMENT_GRAY, COLOR_ERROR_RED, COLOR_EXEC_GREEN, COLOR_PROMPT_ACCENT, shell_write,
    shell_write_idx,
};
use super::super::{HALTED, NL, REBOOTING};
use super::{BUILTINS, BuiltinCategory};

const NAME_COL_WIDTH: usize = 12;
const PADDING: &[u8] = b"            ";

fn write_padded_colored(name: &str, color: u8) {
    shell_write_idx(name.as_bytes(), color);
    let pad = NAME_COL_WIDTH.saturating_sub(name.len());
    if pad > 0 {
        shell_write(&PADDING[..pad]);
    }
}

pub fn cmd_help(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc >= 2 && !argv[1].is_empty() {
        return cmd_help_single(argv[1]);
    }

    shell_write_idx(b"SlopOS Shell v0.2\n", COLOR_PROMPT_ACCENT);
    shell_write(b"Type 'help <command>' for detailed usage.\n\n");

    for &cat in BuiltinCategory::ALL {
        shell_write_idx(cat.label().as_bytes(), COLOR_PROMPT_ACCENT);
        shell_write(b":\n");
        for entry in BUILTINS {
            if entry.category != cat {
                continue;
            }
            shell_write(b"  ");
            write_padded_colored(entry.name, COLOR_EXEC_GREEN);
            shell_write(entry.desc.as_bytes());
            shell_write(NL.as_bytes());
        }
        shell_write(NL.as_bytes());
    }

    shell_write_idx(b"Programs", COLOR_PROMPT_ACCENT);
    shell_write(b":\n");
    for spec in program_registry::user_programs() {
        shell_write(b"  ");
        write_padded_colored(spec.name, COLOR_EXEC_GREEN);
        shell_write(spec.desc.as_bytes());
        shell_write(NL.as_bytes());
    }
    shell_write(NL.as_bytes());

    0
}

fn cmd_help_single(name: &[u8]) -> i32 {
    for entry in BUILTINS {
        if name != entry.name.as_bytes() {
            continue;
        }
        shell_write_idx(entry.name.as_bytes(), COLOR_EXEC_GREEN);
        shell_write(b" - ");
        shell_write(entry.desc.as_bytes());
        shell_write(b"\n\n");
        shell_write_idx(b"Usage: ", COLOR_COMMENT_GRAY);
        shell_write(entry.usage.as_bytes());
        shell_write(b"\n\n");
        if !entry.detail.is_empty() {
            shell_write(entry.detail.as_bytes());
            shell_write(NL.as_bytes());
        }
        return 0;
    }

    for spec in program_registry::user_programs() {
        if name != spec.name.as_bytes() {
            continue;
        }
        shell_write_idx(spec.name.as_bytes(), COLOR_EXEC_GREEN);
        shell_write(b" - ");
        shell_write(spec.desc.as_bytes());
        shell_write(NL.as_bytes());
        return 0;
    }

    shell_write_idx(b"help: unknown command '", COLOR_ERROR_RED);
    shell_write_idx(name, COLOR_ERROR_RED);
    shell_write_idx(b"'\n", COLOR_ERROR_RED);
    1
}

pub fn cmd_echo(argc: i32, argv: &[&[u8]]) -> i32 {
    let mut first = true;
    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() {
            break;
        }
        let arg = argv[idx];
        if arg.is_empty() {
            continue;
        }
        if !first {
            shell_write(b" ");
        }
        shell_write(arg);
        first = false;
    }
    shell_write(NL.as_bytes());
    0
}

pub fn cmd_clear(_argc: i32, _argv: &[&[u8]]) -> i32 {
    shell_write(b"\x1b[2J\x1b[H");
    0
}

/// Ask `/bin/halt` to bring the machine down.
///
/// Spawned rather than issued directly: `halt` and `reboot` need the `Power`
/// capability, which the kernel confers on exactly one program. A shell that
/// held it would put whole-machine authority in the process that runs every
/// command the user types — which is why Linux ships `/sbin/halt` as a
/// separate binary, `systemctl poweroff` asks logind, and Redox puts the
/// resource behind a daemon.
///
/// Returns only on failure; on success the child powers the machine off.
fn spawn_halt(action: &[u8]) -> i32 {
    let argv: [*const u8; 1] = [action.as_ptr()];
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_path_with_actions(
        b"/bin/halt",
        &argv,
        slopos_abi::task::TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        shell_write(b"sh: cannot start /bin/halt\n");
        return 1;
    }
    process::waitpid(tid as u32);
    // Reached only if the child failed: a successful halt never returns.
    1
}

pub fn cmd_shutdown(_argc: i32, _argv: &[&[u8]]) -> i32 {
    shell_write(HALTED.as_bytes());
    spawn_halt(b"halt\0")
}

pub fn cmd_reboot(_argc: i32, _argv: &[&[u8]]) -> i32 {
    shell_write(REBOOTING.as_bytes());
    spawn_halt(b"reboot\0")
}

fn info_kv(label: &[u8], value: impl core::fmt::Display) {
    shell_write_idx(label, COLOR_COMMENT_GRAY);
    shell_write(format!("{value}\n").as_bytes());
}

pub fn cmd_info(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"info: failed\n", COLOR_ERROR_RED);
        return 1;
    }

    shell_write_idx(b"Kernel information:\n", COLOR_PROMPT_ACCENT);

    info_kv(b"  Total pages:      ", info.total_pages);
    info_kv(b"  Free pages:       ", info.free_pages);
    info_kv(b"  Allocated pages:  ", info.allocated_pages);

    info_kv(b"  Total tasks:      ", info.total_tasks);
    info_kv(b"  Active tasks:     ", info.active_tasks);
    info_kv(b"  Ready tasks:      ", info.ready_tasks);

    info_kv(b"  Task switches:    ", info.task_context_switches);
    info_kv(b"  Sched switches:   ", info.scheduler_context_switches);
    info_kv(b"  Sched yields:     ", info.scheduler_yields);
    info_kv(b"  schedule() calls: ", info.schedule_calls);

    0
}

pub fn cmd_uptime(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let ns = sys_core::clock_gettime_ns();
    let total_secs = ns / 1_000_000_000;
    let sub_ms = (ns % 1_000_000_000) / 1_000_000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    let prefix = if hours > 0 {
        format!("up {hours}h {minutes:02}:{seconds:02}.{sub_ms:03}")
    } else {
        format!("up {minutes:02}:{seconds:02}.{sub_ms:03}")
    };
    shell_write(format!("{prefix} ({} ms)\n", ns / 1_000_000).as_bytes());
    0
}

pub fn cmd_cpuinfo(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let cpu_count = sys_core::get_cpu_count();
    let current = sys_core::get_current_cpu();

    shell_write_idx(b"Architecture:  ", COLOR_COMMENT_GRAY);
    shell_write(format!("x86_64\n").as_bytes());
    shell_write_idx(b"CPU(s):        ", COLOR_COMMENT_GRAY);
    shell_write(format!("{cpu_count}\n").as_bytes());
    shell_write_idx(b"Current CPU:   ", COLOR_COMMENT_GRAY);
    shell_write(format!("{current}\n").as_bytes());
    0
}

pub fn cmd_free(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"free: failed to query system info\n", COLOR_ERROR_RED);
        return 1;
    }

    const PAGE_SIZE_KB: u64 = 4;
    let total_kb = info.total_pages as u64 * PAGE_SIZE_KB;
    let free_kb = info.free_pages as u64 * PAGE_SIZE_KB;
    let used_kb = info.allocated_pages as u64 * PAGE_SIZE_KB;

    shell_write_idx(
        b"              total       free       used\n",
        COLOR_COMMENT_GRAY,
    );

    shell_write_idx(b"Pages:   ", COLOR_COMMENT_GRAY);
    shell_write(
        format!(
            "{:>10}{:>11}{:>11}\n",
            info.total_pages, info.free_pages, info.allocated_pages
        )
        .as_bytes(),
    );

    shell_write_idx(b"KiB:     ", COLOR_COMMENT_GRAY);
    shell_write(format!("{total_kb:>10}{free_kb:>11}{used_kb:>11}\n").as_bytes());

    shell_write_idx(b"MiB:     ", COLOR_COMMENT_GRAY);
    shell_write(
        format!(
            "{:>10}{:>11}{:>11}\n",
            total_kb / 1024,
            free_kb / 1024,
            used_kb / 1024
        )
        .as_bytes(),
    );

    0
}

pub fn cmd_time(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"time: missing command\n", COLOR_ERROR_RED);
        return 1;
    }

    let start_ns = sys_core::clock_gettime_ns();

    let mut sub = super::super::buffers::ParsedTokens::new();
    for arg in &argv[1..argc as usize] {
        sub.push_token(arg);
    }
    let rc = super::super::exec::execute_tokens(&sub);

    let end_ns = sys_core::clock_gettime_ns();
    let elapsed_ns = end_ns.saturating_sub(start_ns);

    let secs = elapsed_ns / 1_000_000_000;
    let sub_us = (elapsed_ns % 1_000_000_000) / 1_000;

    shell_write(format!("\nreal\t{secs}.{sub_us:06}s\n").as_bytes());

    rc
}

pub fn cmd_date(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let ms = sys_core::get_time_ms();
    let total_secs = ms / 1000;
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    shell_write(
        format!("Day {days} {hours:02}:{minutes:02}:{seconds:02} SLT (Sloptopia Local Time)\n")
            .as_bytes(),
    );
    0
}

pub fn cmd_uname(argc: i32, argv: &[&[u8]]) -> i32 {
    let mut show_all = argc < 2;
    let mut show_sysname = false;
    let mut show_release = false;
    let mut show_machine = false;

    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() || argv[idx].is_empty() {
            continue;
        }
        if argv[idx] == b"-a" {
            show_all = true;
        } else if argv[idx] == b"-s" {
            show_sysname = true;
        } else if argv[idx] == b"-r" {
            show_release = true;
        } else if argv[idx] == b"-m" {
            show_machine = true;
        }
    }

    if !show_sysname && !show_release && !show_machine {
        show_all = true;
    }

    let mut first = true;

    if show_all || show_sysname {
        shell_write(b"SlopOS");
        first = false;
    }
    if show_all || show_release {
        if !first {
            shell_write(b" ");
        }
        shell_write(b"0.2-slop");
        first = false;
    }
    if show_all || show_machine {
        if !first {
            shell_write(b" ");
        }
        shell_write(b"x86_64");
    }

    shell_write(NL.as_bytes());
    0
}

pub fn cmd_whoami(_argc: i32, _argv: &[&[u8]]) -> i32 {
    let uid = process::getuid();
    if uid == 0 {
        shell_write(b"root\n");
    } else {
        shell_write(format!("uid={uid}\n").as_bytes());
    }
    0
}

pub fn cmd_resolve(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 || argv[1].is_empty() {
        shell_write(b"usage: resolve <hostname>\n");
        return 1;
    }

    let host_str = match core::str::from_utf8(argv[1]) {
        Ok(s) => s,
        Err(_) => {
            shell_write(b"resolve: invalid hostname\n");
            return 1;
        }
    };

    match crate::net::resolve_host(host_str) {
        Ok(addr) => {
            shell_write(format!("{}: -> {}\n", host_str, addr).as_bytes());
            0
        }
        Err(err) => {
            shell_write(format!("resolve: {}: {}\n", host_str, err).as_bytes());
            1
        }
    }
}
