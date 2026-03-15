use crate::syscall::{UserSysInfo, core as sys_core};

use super::display::{
    COLOR_COMMENT_GRAY, COLOR_ERROR_RED, COLOR_EXEC_GREEN, COLOR_PROMPT_ACCENT, COLOR_WARN_YELLOW,
    shell_write, shell_write_idx,
};

const LOGO: &[&[u8]] = &[
    b"   _____ __            ____  _____",
    b"  / ___// /___  ____  / __ \\/ ___/",
    b"  \\__ \\/ / __ \\/ __ \\/ / / /\\__ \\ ",
    b" ___/ / / /_/ / /_/ / /_/ /___/ / ",
    b"/____/_/\\____/ .___/\\____//____/  ",
    b"            /_/                   ",
];

const VERSION: &str = "v0.2-slop";
const ARCH: &str = "x86_64";

pub fn print_welcome_banner() {
    shell_write(b"\n");

    for line in LOGO {
        shell_write_idx(line, COLOR_PROMPT_ACCENT);
        shell_write(b"\n");
    }

    shell_write(b"\n");

    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);
    shell_write_idx(VERSION.as_bytes(), COLOR_EXEC_GREEN);
    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);
    shell_write_idx(ARCH.as_bytes(), COLOR_COMMENT_GRAY);
    shell_write_idx(b"  ", COLOR_COMMENT_GRAY);

    let uptime_ms = sys_core::get_time_ms();
    let total_secs = (uptime_ms / 1000) as u32;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let uptime = if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else {
        format!("{}m {}s", m, s)
    };
    shell_write_idx(b"up ", COLOR_COMMENT_GRAY);
    shell_write_idx(uptime.as_bytes(), COLOR_COMMENT_GRAY);

    shell_write(b"\n");

    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) == 0 {
        let balance = info.wl_balance;

        shell_write_idx(b"  W/L: ", COLOR_COMMENT_GRAY);

        let balance_text = format!("{}", balance);

        let bal_color = if balance > 0 {
            COLOR_EXEC_GREEN
        } else if balance < 0 {
            COLOR_ERROR_RED
        } else {
            COLOR_WARN_YELLOW
        };
        shell_write_idx(balance_text.as_bytes(), bal_color);

        let fate_msg: &str = if balance > 100 {
            "  The Wheel favors the bold."
        } else if balance > 0 {
            "  Fate watches with interest."
        } else if balance == 0 {
            "  Perfectly balanced."
        } else if balance > -100 {
            "  The house is winning."
        } else {
            "  Deep in the red."
        };
        shell_write_idx(fate_msg.as_bytes(), COLOR_COMMENT_GRAY);
        shell_write(b"\n");
    }

    shell_write(b"\n");
}
