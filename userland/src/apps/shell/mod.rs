use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

mod banner;
pub mod buffers;
pub mod builtins;
pub mod completion;
pub mod display;
pub mod env;
pub mod exec;
pub mod history;
pub mod input;
pub mod jobs;
pub mod parser;
mod surface;

pub(crate) static NL: &str = "\n";
pub(crate) static UNKNOWN_CMD: &str = "Unknown command. Type 'help'.\n";
pub(crate) static PATH_TOO_LONG: &str = "path too long\n";
pub(crate) static ERR_NO_SUCH: &str = "No such file or directory\n";
pub(crate) static ERR_TOO_MANY_ARGS: &str = "too many arguments\n";
pub(crate) static ERR_MISSING_OPERAND: &str = "missing operand\n";
pub(crate) static ERR_MISSING_FILE: &str = "missing file operand\n";
pub(crate) static ERR_MISSING_TEXT: &str = "missing text operand\n";
pub(crate) static HALTED: &str = "Shell requested shutdown...\n";
pub(crate) static REBOOTING: &str = "Shell requested reboot...\n";

pub(crate) const SHELL_IO_MAX: usize = 512;

const CWD_MAX: usize = 256;
static CWD: Mutex<[u8; CWD_MAX]> = Mutex::new([0; CWD_MAX]);

static LAST_EXIT_CODE: AtomicI32 = AtomicI32::new(0);
static LAST_BG_PID: AtomicU32 = AtomicU32::new(0);
static SHELL_PID: AtomicU32 = AtomicU32::new(0);
static SHELL_PTY_MASTER: AtomicI32 = AtomicI32::new(-1);
static SHELL_PTY_SLAVE: AtomicI32 = AtomicI32::new(-1);

pub fn cwd_bytes() -> [u8; CWD_MAX] {
    *CWD.lock().unwrap()
}

pub fn cwd_set(path: &[u8]) {
    let cwd = &mut *CWD.lock().unwrap();
    let len = path.len().min(CWD_MAX - 1);
    cwd[..len].copy_from_slice(&path[..len]);
    cwd[len] = 0;
}

pub fn last_exit_code() -> i32 {
    LAST_EXIT_CODE.load(Ordering::Relaxed)
}

pub fn set_last_exit_code(code: i32) {
    LAST_EXIT_CODE.store(code, Ordering::Relaxed)
}

pub fn last_bg_pid() -> u32 {
    LAST_BG_PID.load(Ordering::Relaxed)
}

pub fn set_last_bg_pid(pid: u32) {
    LAST_BG_PID.store(pid, Ordering::Relaxed)
}

pub fn shell_pid() -> u32 {
    SHELL_PID.load(Ordering::Relaxed)
}

pub fn set_shell_pty_pair(master_idx: u32, slave_idx: u32) {
    SHELL_PTY_MASTER.store(master_idx as i32, Ordering::Relaxed);
    SHELL_PTY_SLAVE.store(slave_idx as i32, Ordering::Relaxed);
}

pub fn shell_pty_pair() -> Option<(u32, u32)> {
    let master = SHELL_PTY_MASTER.load(Ordering::Relaxed);
    let slave = SHELL_PTY_SLAVE.load(Ordering::Relaxed);
    if master < 0 || slave < 0 {
        None
    } else {
        Some((master as u32, slave as u32))
    }
}

/// Return the PTY master TTY index (not a file descriptor), or -1 if unavailable.
///
/// This is a kernel-internal TTY slot index used with `tty_write`/`tty_read`,
/// not a POSIX file descriptor.  See [`SHELL_PTY_MASTER_FD`] for the fd.
pub fn shell_pty_master() -> i32 {
    SHELL_PTY_MASTER.load(Ordering::Relaxed)
}

/// Raw file descriptor for the PTY master device.
///
/// Distinct from [`SHELL_PTY_MASTER`] which stores a kernel TTY slot index.
/// This fd is obtained from `open_tty_fd` and can be used with `read`/`write`/`close`.
static SHELL_PTY_MASTER_FD: AtomicI32 = AtomicI32::new(-1);

/// Store the raw file descriptor for the PTY master device.
fn set_shell_pty_master_fd(fd: i32) {
    SHELL_PTY_MASTER_FD.store(fd, Ordering::Relaxed);
}

pub(crate) const PROMPT_BUF_MAX: usize = 280;

// Fallback: matches the previous hardcoded `[/path] $ ` format.
const DEFAULT_PS1: &[u8] = b"[\\w] \\$ ";

/// Expand PS1 escape sequences (`\w` `\u` `\h` `\$` `\t` `\n` `\\`) into
/// `text_buf`/`color_buf`. Returns bytes written.
fn expand_ps1(
    ps1: &[u8],
    text_buf: &mut [u8; PROMPT_BUF_MAX],
    color_buf: &mut [u8; PROMPT_BUF_MAX],
) -> usize {
    use crate::syscall::core as sys_core;
    use crate::syscall::process;
    use display::{
        COLOR_COMMENT_GRAY, COLOR_DEFAULT, COLOR_EXEC_GREEN, COLOR_PATH_BLUE, COLOR_PROMPT_ACCENT,
    };

    let mut out = 0usize;
    let mut i = 0usize;

    while i < ps1.len() && out < PROMPT_BUF_MAX {
        if ps1[i] == b'\\' && i + 1 < ps1.len() {
            i += 1;
            match ps1[i] {
                b'w' => {
                    let cwd = CWD.lock().unwrap();
                    let cwd: &[u8] = &*cwd;
                    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(0);
                    let avail = PROMPT_BUF_MAX - out;
                    let copy = cwd_len.min(avail);
                    text_buf[out..out + copy].copy_from_slice(&cwd[..copy]);
                    fill_color(color_buf, out, copy, COLOR_PATH_BLUE);
                    out += copy;
                }
                b'u' => {
                    out += emit_segment(text_buf, color_buf, out, b"root", COLOR_EXEC_GREEN);
                }
                b'h' => {
                    out += emit_segment(text_buf, color_buf, out, b"sloptopia", COLOR_EXEC_GREEN);
                }
                b'$' => {
                    let ch = if process::getuid() == 0 { b'#' } else { b'$' };
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = ch;
                        color_buf[out] = COLOR_PROMPT_ACCENT;
                        out += 1;
                    }
                }
                b't' => {
                    let ms = sys_core::get_time_ms();
                    let total_secs = (ms / 1000) as u32;
                    let h = total_secs / 3600;
                    let m = (total_secs % 3600) / 60;
                    let s = total_secs % 60;
                    let mut time_buf = [0u8; 8];
                    time_buf[0] = b'0' + (h / 10 % 10) as u8;
                    time_buf[1] = b'0' + (h % 10) as u8;
                    time_buf[2] = b':';
                    time_buf[3] = b'0' + (m / 10) as u8;
                    time_buf[4] = b'0' + (m % 10) as u8;
                    time_buf[5] = b':';
                    time_buf[6] = b'0' + (s / 10) as u8;
                    time_buf[7] = b'0' + (s % 10) as u8;
                    out += emit_segment(text_buf, color_buf, out, &time_buf, COLOR_COMMENT_GRAY);
                }
                b'n' => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\n';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
                b'\\' => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\\';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
                other => {
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = b'\\';
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                    if out < PROMPT_BUF_MAX {
                        text_buf[out] = other;
                        color_buf[out] = COLOR_DEFAULT;
                        out += 1;
                    }
                }
            }
            i += 1;
        } else {
            text_buf[out] = ps1[i];
            color_buf[out] = display::COLOR_DEFAULT;
            out += 1;
            i += 1;
        }
    }

    out
}

#[inline]
fn emit_segment(
    buf: &mut [u8; PROMPT_BUF_MAX],
    colors: &mut [u8; PROMPT_BUF_MAX],
    offset: usize,
    segment: &[u8],
    color_idx: u8,
) -> usize {
    let avail = PROMPT_BUF_MAX - offset;
    let copy = segment.len().min(avail);
    buf[offset..offset + copy].copy_from_slice(&segment[..copy]);
    fill_color(colors, offset, copy, color_idx);
    copy
}

#[inline]
fn fill_color(colors: &mut [u8; PROMPT_BUF_MAX], offset: usize, count: usize, color_idx: u8) {
    let end = (offset + count).min(PROMPT_BUF_MAX);
    for slot in &mut colors[offset..end] {
        *slot = color_idx;
    }
}

fn build_prompt(
    text_buf: &mut [u8; PROMPT_BUF_MAX],
    color_buf: &mut [u8; PROMPT_BUF_MAX],
) -> usize {
    let ps1 = env::get(b"PS1");
    match ps1 {
        Some((val, len)) => expand_ps1(&val[..len], text_buf, color_buf),
        None => expand_ps1(DEFAULT_PS1, text_buf, color_buf),
    }
}

fn write_colored_prompt(prompt: &[u8], colors: &[u8]) {
    use display::{COLOR_DEFAULT, shell_console_commit, shell_console_write_colored};

    let _ = crate::syscall::tty::write(prompt);

    let mut i = 0;
    while i < prompt.len() {
        let color = if i < colors.len() {
            colors[i]
        } else {
            COLOR_DEFAULT
        };
        let start = i;
        while i < prompt.len() && (i >= colors.len() || colors[i] == color) {
            i += 1;
        }
        shell_console_write_colored(&prompt[start..i], color);
    }
    shell_console_commit();
}

pub struct ShellState {
    pub prompt_buf: [u8; PROMPT_BUF_MAX],
    pub prompt_colors: [u8; PROMPT_BUF_MAX],
    pub prompt_len: usize,
}

pub fn shell_user_main() {
    shell_interactive_main();
}

fn shell_interactive_main() {
    use slopos_abi::signal::SIGINT;

    use crate::syscall::{fs, process};
    use slopos_windowing::connection;

    let handle = connection::connect().expect("compositor not running");
    surface::init_handle(handle.clone());
    input::init_handle(handle);

    display::shell_console_init();
    display::shell_console_clear();

    surface::set_title("SlopOS Shell");
    surface::set_app_id("org.slopos.shell");
    surface::set_cursor_shape(slopos_abi::CURSOR_SHAPE_TEXT);

    cwd_set(b"/");
    env::initialize_defaults();
    SHELL_PID.store(std::process::id(), Ordering::Relaxed);
    if let Ok((master, slave)) = process::openpty() {
        set_shell_pty_pair(master, slave);
        if let Ok(master_fd) = process::open_tty_fd(master) {
            if let Ok(slave_fd) = fs::ioctl_tiocgptpeer(master_fd.raw()) {
                set_shell_pty_master_fd(master_fd.into_raw());
                let _ = fs::dup2(slave_fd.raw(), 0);
                let _ = fs::dup2(slave_fd.raw(), 1);
                let _ = fs::dup2(slave_fd.raw(), 2);
            }
        }
    }
    exec::initialize_job_control();
    let _ = process::ignore_signal(SIGINT);

    banner::print_welcome_banner();

    let mut state = ShellState {
        prompt_buf: [0; PROMPT_BUF_MAX],
        prompt_colors: [0; PROMPT_BUF_MAX],
        prompt_len: 0,
    };

    loop {
        jobs::notify_completed_jobs();
        state.prompt_len = build_prompt(&mut state.prompt_buf, &mut state.prompt_colors);
        let prompt = &state.prompt_buf[..state.prompt_len];

        write_colored_prompt(prompt, &state.prompt_colors[..state.prompt_len]);

        let mut tokens = buffers::ParsedTokens::new();
        let prompt_colors = &state.prompt_colors[..state.prompt_len];
        let token_count = input::read_command_line(&mut tokens, prompt, prompt_colors);

        if token_count <= 0 {
            continue;
        }

        let rc = exec::execute_tokens(&tokens);
        set_last_exit_code(rc);
        if rc == 127 {
            display::shell_write_idx(UNKNOWN_CMD.as_bytes(), display::COLOR_ERROR_RED);
        }
    }
}
