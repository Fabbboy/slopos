use core::ffi::c_char;
use core::ptr;

use crate::program_registry;
use crate::syscall::{POLLIN, UserFsStat, UserPollFd, core as sys_core, fs, process};
use slopos_abi::fs::{O_APPEND, O_CREAT, O_RDONLY, O_WRONLY};

use super::buffers::ParsedTokens;
use super::builtins;
use super::display::{shell_clear_output_fd, shell_set_output_fd, shell_write};
use super::jobs;
use super::parser::{SHELL_MAX_TOKENS, normalize_path};
use std::sync::atomic::{AtomicU32, Ordering};

const MAX_PIPE_CMDS: usize = 8;
const MAX_REDIRECTS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Input,
    OutputTruncate,
    OutputAppend,
}

#[derive(Clone, Copy)]
struct Redirect {
    kind: RedirectKind,
    target: usize, // index into ParsedTokens
}

impl Redirect {
    const fn empty() -> Self {
        Self {
            kind: RedirectKind::Input,
            target: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct ParsedCommand {
    argv: [usize; SHELL_MAX_TOKENS], // indices into ParsedTokens
    argc: usize,
    redirects: [Redirect; MAX_REDIRECTS],
    redirect_count: usize,
}

impl ParsedCommand {
    const fn empty() -> Self {
        Self {
            argv: [0; SHELL_MAX_TOKENS],
            argc: 0,
            redirects: [Redirect::empty(); MAX_REDIRECTS],
            redirect_count: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct ParsedPipeline {
    commands: [ParsedCommand; MAX_PIPE_CMDS],
    command_count: usize,
    background: bool,
}

impl ParsedPipeline {
    const fn empty() -> Self {
        Self {
            commands: [ParsedCommand::empty(); MAX_PIPE_CMDS],
            command_count: 0,
            background: false,
        }
    }
}

#[derive(Clone, Copy)]
struct SavedFd {
    fd: i32,
    backup: i32,
}

impl SavedFd {
    const fn empty() -> Self {
        Self { fd: -1, backup: -1 }
    }
}

static FOREGROUND_PGID: AtomicU32 = AtomicU32::new(0);
static SHELL_PGID: AtomicU32 = AtomicU32::new(0);

pub fn foreground_pgid() -> u32 {
    FOREGROUND_PGID.load(Ordering::Relaxed)
}

pub fn set_foreground_pgid(pgid: u32) {
    FOREGROUND_PGID.store(pgid, Ordering::Relaxed);
}

pub fn clear_foreground_pgid() {
    FOREGROUND_PGID.store(0, Ordering::Relaxed);
}

pub fn initialize_job_control() {
    let _ = process::setsid();
    let _ = fs::tiocsctty(0);

    process::ignore_signal(slopos_abi::signal::SIGTTOU);
    process::ignore_signal(slopos_abi::signal::SIGTTIN);
    process::ignore_signal(slopos_abi::signal::SIGTSTP);

    let _ = process::setpgid(0, 0);
    let shell_pgid = process::getpgid(0);
    if shell_pgid > 0 {
        SHELL_PGID.store(shell_pgid as u32, Ordering::Relaxed);
        let _ = fs::tcsetpgrp(0, shell_pgid as u32);
    }
}

fn shell_pgid() -> u32 {
    SHELL_PGID.load(Ordering::Relaxed)
}

pub fn enter_foreground(pgid: u32) {
    if pgid == 0 {
        return;
    }
    set_foreground_pgid(pgid);
    let _ = fs::tcsetpgrp(0, pgid);
}

pub fn leave_foreground() {
    let pgid = shell_pgid();
    if pgid != 0 {
        let _ = fs::tcsetpgrp(0, pgid);
    }
    clear_foreground_pgid();
}

fn parse_pipeline(tokens: &ParsedTokens, out: &mut ParsedPipeline) -> Result<(), ()> {
    *out = ParsedPipeline::empty();
    let argc = tokens.count();
    if argc == 0 {
        return Err(());
    }

    let mut cmd_idx = 0usize;
    let mut token_idx = 0usize;

    while token_idx < argc {
        let tok = tokens.token(token_idx);

        if tok == b"&" {
            if token_idx + 1 != argc {
                return Err(());
            }
            out.background = true;
            token_idx += 1;
            continue;
        }

        if tok == b"|" {
            if out.commands[cmd_idx].argc == 0 {
                return Err(());
            }
            cmd_idx += 1;
            if cmd_idx >= MAX_PIPE_CMDS {
                return Err(());
            }
            token_idx += 1;
            continue;
        }

        let mut redirect_kind = None;
        if tok == b">" {
            redirect_kind = Some(RedirectKind::OutputTruncate);
        } else if tok == b">>" {
            redirect_kind = Some(RedirectKind::OutputAppend);
        } else if tok == b"<" {
            redirect_kind = Some(RedirectKind::Input);
        }

        if let Some(kind) = redirect_kind {
            if token_idx + 1 >= argc {
                return Err(());
            }
            if out.commands[cmd_idx].redirect_count >= MAX_REDIRECTS {
                return Err(());
            }
            let target_idx = token_idx + 1;
            let redir_idx = out.commands[cmd_idx].redirect_count;
            out.commands[cmd_idx].redirects[redir_idx] = Redirect {
                kind,
                target: target_idx,
            };
            out.commands[cmd_idx].redirect_count += 1;
            token_idx += 2;
            continue;
        }

        let cmd = &mut out.commands[cmd_idx];
        if cmd.argc >= SHELL_MAX_TOKENS - 1 {
            return Err(());
        }
        cmd.argv[cmd.argc] = token_idx;
        cmd.argc += 1;
        token_idx += 1;
    }

    if out.commands[cmd_idx].argc == 0 {
        return Err(());
    }

    out.command_count = cmd_idx + 1;
    Ok(())
}

fn resolve_via_path(name: &[u8], tmp: &mut [u8; 256]) -> bool {
    use super::env;

    let Some((path_val, path_len)) = env::get(b"PATH") else {
        return false;
    };
    if path_len == 0 {
        return false;
    }

    let mut seg_start = 0usize;
    while seg_start < path_len {
        let mut seg_end = seg_start;
        while seg_end < path_len && path_val[seg_end] != b':' {
            seg_end += 1;
        }
        let dir = &path_val[seg_start..seg_end];
        if !dir.is_empty() {
            let needs_sep = dir[dir.len() - 1] != b'/';
            let total = dir.len() + if needs_sep { 1 } else { 0 } + name.len();
            if total < tmp.len() {
                let mut pos = 0usize;
                tmp[pos..pos + dir.len()].copy_from_slice(dir);
                pos += dir.len();
                if needs_sep {
                    tmp[pos] = b'/';
                    pos += 1;
                }
                tmp[pos..pos + name.len()].copy_from_slice(name);
                pos += name.len();
                tmp[pos] = 0;

                let mut stat = UserFsStat::default();
                if fs::stat_path(tmp.as_ptr() as *const c_char, &mut stat).is_ok() {
                    return true;
                }
            }
        }
        seg_start = seg_end + 1;
    }
    false
}

fn resolve_exec_path(name: &[u8], tmp: &mut [u8; 256]) -> bool {
    if name.is_empty() {
        return false;
    }

    if name.contains(&b'/') {
        if normalize_path(name, tmp) != 0 {
            return false;
        }
        let mut stat = UserFsStat::default();
        if fs::stat_path(tmp.as_ptr() as *const c_char, &mut stat).is_err() {
            return false;
        }
        return true;
    }

    if let Ok(name_str) = core::str::from_utf8(name)
        && let Some(spec) = program_registry::resolve_program(name_str)
    {
        let path_bytes = spec.path.as_bytes();
        let path_len = path_bytes.len().min(tmp.len() - 1);
        tmp[..path_len].copy_from_slice(&path_bytes[..path_len]);
        tmp[path_len] = 0;
        return true;
    }

    resolve_via_path(name, tmp)
}

fn is_builtin_command(cmd: &ParsedCommand, tokens: &ParsedTokens) -> bool {
    if cmd.argc == 0 {
        return false;
    }
    builtins::find_builtin(tokens.token(cmd.argv[0])).is_some()
}

fn is_passthrough_cat(cmd: &ParsedCommand, tokens: &ParsedTokens) -> bool {
    cmd.redirect_count == 0 && cmd.argc == 1 && tokens.token(cmd.argv[0]) == b"cat"
}

fn simplify_pipeline(pipeline: &mut ParsedPipeline, tokens: &ParsedTokens) {
    if pipeline.command_count <= 1 {
        return;
    }

    let mut compacted = [ParsedCommand::empty(); MAX_PIPE_CMDS];
    let mut out = 0usize;
    for i in 0..pipeline.command_count {
        let cmd = pipeline.commands[i];
        if i > 0 && is_passthrough_cat(&cmd, tokens) {
            continue;
        }
        compacted[out] = cmd;
        out += 1;
    }

    if out > 0 {
        pipeline.commands = compacted;
        pipeline.command_count = out;
    }
}

fn command_name_bytes<'a>(cmd: &ParsedCommand, tokens: &'a ParsedTokens) -> Option<&'a [u8]> {
    if cmd.argc == 0 {
        return None;
    }
    let name = tokens.token(cmd.argv[0]);
    if name.is_empty() {
        return None;
    }
    Some(name)
}

fn registry_spec_for_command(
    cmd: &ParsedCommand,
    tokens: &ParsedTokens,
) -> Option<&'static program_registry::ProgramSpec> {
    let name = command_name_bytes(cmd, tokens)?;
    if name.contains(&b'/') {
        let mut tmp = [0u8; 256];
        if normalize_path(name, &mut tmp) != 0 {
            return None;
        }
        let path_len = tmp.iter().position(|&b| b == 0).unwrap_or(tmp.len());
        let path_str = core::str::from_utf8(&tmp[..path_len]).ok()?;
        return program_registry::resolve_program_path(path_str);
    }
    let name_str = core::str::from_utf8(name).ok()?;
    program_registry::resolve_program(name_str)
}

fn command_resolves(cmd: &ParsedCommand, tokens: &ParsedTokens) -> bool {
    if cmd.argc == 0 {
        return false;
    }
    if is_builtin_command(cmd, tokens) {
        return true;
    }
    if registry_spec_for_command(cmd, tokens).is_some() {
        return true;
    }
    let name = tokens.token(cmd.argv[0]);
    let mut tmp = [0u8; 256];
    resolve_exec_path(name, &mut tmp)
}

fn print_background_job_started(job_id: u16, pid: u32) {
    super::set_last_bg_pid(pid);
    shell_write(b"[");
    jobs::write_u64(job_id as u64);
    shell_write(b"] ");
    jobs::write_u64(pid as u64);
    shell_write(b"\n");
}

/// Forward compositor keyboard events to the PTY master so the slave's
/// line discipline handles ISIG signal generation, echo, and canonical
/// editing.  Also drains PTY master output (echo bytes) to the display.
///
/// This replaces the old `drain_compositor_signals()` which bypassed the
/// TTY and called `kill()` directly — breaking ISIG semantics, preventing
/// child stdin reads, and only handling Ctrl+C (not Ctrl+Z/Ctrl+\).
fn forward_compositor_keyboard() {
    use crate::syscall::{InputEvent, InputEventType};

    let master = super::shell_pty_master();
    if master < 0 {
        return;
    }
    let master_idx = master as u32;

    // Forward keyboard bytes to PTY master → slave ldisc processes them.
    let mut events = [InputEvent::default(); 8];
    let count = super::input::poll_protocol_events(&mut events);
    for i in 0..count.min(8) {
        if events[i].event_type == InputEventType::Configure {
            let new_w = events[i].configure_width() as i32;
            let new_h = events[i].configure_height() as i32;
            super::display::shell_console_resize(new_w, new_h);
        }
        if events[i].event_type == InputEventType::KeyPress {
            let ascii = events[i].key_ascii();
            if ascii != 0 {
                let _ = process::tty_write(master_idx, &[ascii]);
            } else {
                // Non-ASCII keys: convert scancode to VT100 escape sequence.
                let seq: &[u8] = match events[i].key_scancode() {
                    0x82 => b"\x1b[A",  // Up
                    0x83 => b"\x1b[B",  // Down
                    0x84 => b"\x1b[D",  // Left
                    0x85 => b"\x1b[C",  // Right
                    0x86 => b"\x1b[H",  // Home
                    0x87 => b"\x1b[F",  // End
                    0x88 => b"\x1b[3~", // Delete
                    0x80 => b"\x1b[5~", // Page Up
                    0x81 => b"\x1b[6~", // Page Down
                    _ => &[],
                };
                if !seq.is_empty() {
                    let _ = process::tty_write(master_idx, seq);
                }
            }
        }
    }

    // Drain echo/output from the PTY master and display it.
    let mut echo_buf = [0u8; 256];
    let n = process::tty_read(master_idx, &mut echo_buf);
    if n > 0 {
        super::display::shell_write(&echo_buf[..n as usize]);
    }
}

fn execute_registry_spawn(
    cmd: &ParsedCommand,
    tokens: &ParsedTokens,
    background: bool,
) -> Option<i32> {
    if cmd.redirect_count != 0 {
        return None;
    }
    let spec = registry_spec_for_command(cmd, tokens)?;
    let capture = !spec.gui && !background;
    let mut pipe_fds = [-1i32; 2];
    let mut backup_fd = -1i32;

    if capture {
        let (r, w) = match fs::pipe() {
            Ok(pair) => pair,
            Err(_) => {
                shell_write(b"pipe failed\n");
                return Some(1);
            }
        };
        pipe_fds[0] = r.into_raw();
        pipe_fds[1] = w.into_raw();
        backup_fd = match fs::dup(1) {
            Ok(fd) => fd.into_raw(),
            Err(_) => {
                let _ = fs::close_fd_raw(pipe_fds[0]);
                let _ = fs::close_fd_raw(pipe_fds[1]);
                shell_write(b"dup failed\n");
                return Some(1);
            }
        };
        if fs::dup2(pipe_fds[1], 1).is_err() {
            let _ = fs::close_fd_raw(pipe_fds[0]);
            let _ = fs::close_fd_raw(pipe_fds[1]);
            let _ = fs::close_fd_raw(backup_fd);
            shell_write(b"dup2 failed\n");
            return Some(1);
        }
        // Close extra write-end; child will inherit fd 1 = pipe write.
        let _ = fs::close_fd_raw(pipe_fds[1]);
    }

    let spawn_flags = if background {
        spec.flags
    } else {
        spec.flags | slopos_abi::task::TASK_FLAG_NEW_PGRP
    };

    // Build null-terminated argv copies for the syscall ABI boundary.
    // Each token is copied into its own stack buffer so the kernel's
    // copy_bytes_from_user (which reads up to EXEC_MAX_ARG_STRLEN bytes
    // from the pointer) stays within mapped memory.
    let mut arg_bufs = [[0u8; super::parser::SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS];
    let mut argv_ptrs = [ptr::null::<u8>(); SHELL_MAX_TOKENS + 1];
    for i in 0..cmd.argc {
        let tok = tokens.token(cmd.argv[i]);
        let len = tok.len().min(super::parser::SHELL_MAX_TOKEN_LENGTH - 1);
        arg_bufs[i][..len].copy_from_slice(&tok[..len]);
        arg_bufs[i][len] = 0;
        argv_ptrs[i] = arg_bufs[i].as_ptr();
    }

    let tid = process::spawn_path_with_argv(
        spec.path.as_bytes(),
        &argv_ptrs[..cmd.argc],
        spec.priority,
        spawn_flags,
    );

    if capture {
        let _ = fs::dup2(backup_fd, 1);
        let _ = fs::close_fd_raw(backup_fd);
    }
    if tid <= 0 {
        if capture {
            let _ = fs::close_fd_raw(pipe_fds[0]);
        }
        shell_write(b"spawn failed\n");
        return Some(1);
    }
    let pid = tid as u32;
    if background {
        let mut cmd_buf = [0u8; 128];
        let mut len = 0usize;
        if let Some(name) = command_name_bytes(cmd, tokens) {
            let n = name.len().min(cmd_buf.len());
            cmd_buf[..n].copy_from_slice(&name[..n]);
            len = n;
        }
        if let Some(job_id) = jobs::add(pid, pid, &cmd_buf[..len]) {
            print_background_job_started(job_id, pid);
        } else {
            shell_write(b"jobs: table full\n");
        }
        return Some(0);
    }

    // Confirm pgid (child already has it via TASK_FLAG_NEW_PGRP) and set fg.
    let _ = process::setpgid(pid, pid);
    enter_foreground(pid);

    // Stream child stdout to the shell display in real-time.
    // The pipe read-end MUST be non-blocking so the inner drain loop
    // returns EAGAIN when all available data is consumed instead of
    // blocking forever (which would hang for long-running children like nc).
    let status = if capture {
        let _ = crate::syscall::net::set_nonblocking(pipe_fds[0]);
        let mut buf = [0u8; 512];
        let exit_status;
        loop {
            let mut pfds = [UserPollFd {
                fd: pipe_fds[0],
                events: POLLIN,
                revents: 0,
            }];
            let _ = fs::poll(&mut pfds, 10);

            forward_compositor_keyboard();

            loop {
                match fs::read_slice(pipe_fds[0], &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        shell_write(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }

            if let Some(st) = process::waitpid_nohang(pid) {
                loop {
                    match fs::read_slice(pipe_fds[0], &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            shell_write(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
                exit_status = st;
                break;
            }
        }
        let _ = fs::close_fd_raw(pipe_fds[0]);
        exit_status
    } else {
        loop {
            if let Some(st) = process::waitpid_nohang(pid) {
                break st;
            }
            forward_compositor_keyboard();
            sys_core::sleep_ms(5);
        }
    };
    leave_foreground();
    Some(status)
}

fn open_redirect_target(
    redir: Redirect,
    tokens: &ParsedTokens,
    path_buf: &mut [u8; 256],
) -> Result<(i32, i32), ()> {
    if normalize_path(tokens.token(redir.target), path_buf) != 0 {
        return Err(());
    }

    match redir.kind {
        RedirectKind::Input => {
            let fd = fs::open_path(path_buf.as_ptr() as *const c_char, O_RDONLY).map_err(|_| ())?;
            Ok((0, fd.into_raw()))
        }
        RedirectKind::OutputTruncate => {
            let _ = fs::unlink_path(path_buf.as_ptr() as *const c_char);
            let fd = fs::open_path(path_buf.as_ptr() as *const c_char, O_WRONLY | O_CREAT)
                .map_err(|_| ())?;
            Ok((1, fd.into_raw()))
        }
        RedirectKind::OutputAppend => {
            let fd = fs::open_path(
                path_buf.as_ptr() as *const c_char,
                O_WRONLY | O_CREAT | O_APPEND,
            )
            .map_err(|_| ())?;
            Ok((1, fd.into_raw()))
        }
    }
}

fn apply_redirects_for_builtin(
    cmd: &ParsedCommand,
    tokens: &ParsedTokens,
    saved: &mut [SavedFd; MAX_REDIRECTS],
    output_fd: &mut i32,
) -> bool {
    let mut path_buf = [0u8; 256];
    let mut save_count = 0usize;

    for redir in &cmd.redirects[..cmd.redirect_count] {
        let Ok((target_fd, opened_fd)) = open_redirect_target(*redir, tokens, &mut path_buf) else {
            shell_write(b"redirection failed\n");
            return false;
        };

        if target_fd == 1 {
            if *output_fd >= 0 {
                let _ = fs::close_fd_raw(*output_fd);
            }
            *output_fd = opened_fd;
            continue;
        }

        let backup = match fs::dup(target_fd) {
            Ok(fd) => fd.into_raw(),
            Err(_) => {
                let _ = fs::close_fd_raw(opened_fd);
                shell_write(b"redirection failed\n");
                return false;
            }
        };

        if fs::dup2(opened_fd, target_fd).is_err() {
            let _ = fs::close_fd_raw(opened_fd);
            let _ = fs::close_fd_raw(backup);
            shell_write(b"redirection failed\n");
            return false;
        }
        let _ = fs::close_fd_raw(opened_fd);

        if save_count < saved.len() {
            saved[save_count] = SavedFd {
                fd: target_fd,
                backup,
            };
            save_count += 1;
        }
    }

    true
}

fn restore_redirects(saved: &mut [SavedFd; MAX_REDIRECTS]) {
    for slot in saved {
        if slot.fd < 0 || slot.backup < 0 {
            continue;
        }
        let _ = fs::dup2(slot.backup, slot.fd);
        let _ = fs::close_fd_raw(slot.backup);
        *slot = SavedFd::empty();
    }
}

fn command_text(pipeline: &ParsedPipeline, tokens: &ParsedTokens, out: &mut [u8; 128]) -> usize {
    let mut pos = 0usize;
    for ci in 0..pipeline.command_count {
        let cmd = &pipeline.commands[ci];
        for ai in 0..cmd.argc {
            let bytes = tokens.token(cmd.argv[ai]);
            for &b in bytes {
                if pos >= out.len() {
                    return pos;
                }
                out[pos] = b;
                pos += 1;
            }
            if pos < out.len() {
                out[pos] = b' ';
                pos += 1;
            }
        }
        if ci + 1 < pipeline.command_count {
            if pos + 1 >= out.len() {
                return pos;
            }
            out[pos] = b'|';
            pos += 1;
            out[pos] = b' ';
            pos += 1;
        }
    }
    if pos > 0 && out[pos - 1] == b' ' {
        pos -= 1;
    }
    pos
}

fn run_in_child(
    cmd: &ParsedCommand,
    tokens: &ParsedTokens,
    stdin_fd: i32,
    stdout_fd: i32,
    pipes: &[[i32; 2]; MAX_PIPE_CMDS],
    pipe_count: usize,
    pgid: u32,
) -> ! {
    if pgid == 0 {
        let _ = process::setpgid(0, 0);
    } else {
        let _ = process::setpgid(0, pgid);
    }

    if stdin_fd >= 0 {
        if fs::dup2(stdin_fd, 0).is_err() {
            let _ = crate::syscall::tty::write(b"dup2 stdin failed\n");
            sys_core::exit_with_code(1);
        }
    }
    if stdout_fd >= 0 {
        if fs::dup2(stdout_fd, 1).is_err() {
            let _ = crate::syscall::tty::write(b"dup2 stdout failed\n");
            sys_core::exit_with_code(1);
        }
    }

    for pipe in pipes.iter().take(pipe_count) {
        let _ = fs::close_fd_raw(pipe[0]);
        let _ = fs::close_fd_raw(pipe[1]);
    }

    let cmd_name = tokens.token(cmd.argv[0]);
    if let Some(entry) = builtins::find_builtin(cmd_name) {
        let mut builtin_output_fd = 1;
        let mut path_buf = [0u8; 256];
        for redir in &cmd.redirects[..cmd.redirect_count] {
            let Ok((target_fd, opened_fd)) = open_redirect_target(*redir, tokens, &mut path_buf)
            else {
                let _ = crate::syscall::tty::write(b"redirection failed\n");
                sys_core::exit_with_code(1);
            };
            if target_fd == 1 {
                if builtin_output_fd != 1 {
                    let _ = fs::close_fd_raw(builtin_output_fd);
                }
                builtin_output_fd = opened_fd;
                continue;
            }
            if fs::dup2(opened_fd, target_fd).is_err() {
                let _ = crate::syscall::tty::write(b"redirection failed\n");
                let _ = fs::close_fd_raw(opened_fd);
                sys_core::exit_with_code(1);
            }
            let _ = fs::close_fd_raw(opened_fd);
        }

        shell_set_output_fd(builtin_output_fd);
        let mut argv_slices: [&[u8]; SHELL_MAX_TOKENS] = [&[]; SHELL_MAX_TOKENS];
        for i in 0..cmd.argc {
            argv_slices[i] = tokens.token(cmd.argv[i]);
        }
        let code = (entry.func)(cmd.argc as i32, &argv_slices[..cmd.argc]);
        shell_clear_output_fd();
        if builtin_output_fd != 1 {
            let _ = fs::close_fd_raw(builtin_output_fd);
        }
        sys_core::exit_with_code(code);
    }

    let mut path_buf = [0u8; 256];
    for redir in &cmd.redirects[..cmd.redirect_count] {
        let Ok((target_fd, opened_fd)) = open_redirect_target(*redir, tokens, &mut path_buf) else {
            let _ = crate::syscall::tty::write(b"redirection failed\n");
            sys_core::exit_with_code(1);
        };
        if fs::dup2(opened_fd, target_fd).is_err() {
            let _ = crate::syscall::tty::write(b"redirection failed\n");
            let _ = fs::close_fd_raw(opened_fd);
            sys_core::exit_with_code(1);
        }
        let _ = fs::close_fd_raw(opened_fd);
    }

    let name = tokens.token(cmd.argv[0]);
    if !resolve_exec_path(name, &mut path_buf) {
        sys_core::exit_with_code(127);
    }

    // Build raw-pointer argv for the execve syscall ABI boundary.
    // We write each token into null-terminated stack buffers so the pointers
    // remain valid through the syscall.
    let mut arg_bufs = [[0u8; super::parser::SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS];
    let mut argv_ptrs = [ptr::null::<u8>(); SHELL_MAX_TOKENS + 1];
    for i in 0..cmd.argc {
        let tok = tokens.token(cmd.argv[i]);
        let len = tok.len().min(super::parser::SHELL_MAX_TOKEN_LENGTH - 1);
        arg_bufs[i][..len].copy_from_slice(&tok[..len]);
        arg_bufs[i][len] = 0;
        argv_ptrs[i] = arg_bufs[i].as_ptr();
    }
    argv_ptrs[cmd.argc] = ptr::null();

    let rc = process::execve(path_buf.as_ptr(), argv_ptrs.as_ptr(), ptr::null());
    if rc < 0 {
        let _ = crate::syscall::tty::write(b"exec failed\n");
    }
    sys_core::exit_with_code(127);
}

fn execute_single_builtin(cmd: &ParsedCommand, tokens: &ParsedTokens) -> i32 {
    let mut saved = [SavedFd::empty(); MAX_REDIRECTS];
    let mut output_fd = -1;
    if !apply_redirects_for_builtin(cmd, tokens, &mut saved, &mut output_fd) {
        restore_redirects(&mut saved);
        if output_fd >= 0 {
            let _ = fs::close_fd_raw(output_fd);
        }
        return 1;
    }

    let code = if let Some(entry) = builtins::find_builtin(tokens.token(cmd.argv[0])) {
        if output_fd >= 0 {
            shell_set_output_fd(output_fd);
        }
        let mut argv_slices: [&[u8]; SHELL_MAX_TOKENS] = [&[]; SHELL_MAX_TOKENS];
        for i in 0..cmd.argc {
            argv_slices[i] = tokens.token(cmd.argv[i]);
        }
        let rc = (entry.func)(cmd.argc as i32, &argv_slices[..cmd.argc]);
        if output_fd >= 0 {
            shell_clear_output_fd();
        }
        rc
    } else {
        1
    };

    restore_redirects(&mut saved);
    if output_fd >= 0 {
        let _ = fs::close_fd_raw(output_fd);
    }
    code
}

fn execute_pipeline(pipeline: &ParsedPipeline, tokens: &ParsedTokens) -> i32 {
    let inter_pipes = pipeline.command_count.saturating_sub(1);
    let capture_output = !pipeline.background;

    let total_pipes = inter_pipes + if capture_output { 1 } else { 0 };
    let mut pipes = [[-1; 2]; MAX_PIPE_CMDS];
    for pair in pipes.iter_mut().take(total_pipes) {
        match fs::pipe() {
            Ok((r, w)) => {
                pair[0] = r.into_raw();
                pair[1] = w.into_raw();
            }
            Err(_) => {
                shell_write(b"pipe failed\n");
                for p in pipes.iter().take(total_pipes) {
                    if p[0] >= 0 {
                        let _ = fs::close_fd_raw(p[0]);
                    }
                    if p[1] >= 0 {
                        let _ = fs::close_fd_raw(p[1]);
                    }
                }
                return 1;
            }
        }
    }

    let mut pids = [0u32; MAX_PIPE_CMDS];
    let mut pgid = 0u32;

    for i in 0..pipeline.command_count {
        let stdin_fd = if i > 0 { pipes[i - 1][0] } else { -1 };
        let stdout_fd = if i < inter_pipes {
            pipes[i][1]
        } else if capture_output {
            pipes[inter_pipes][1]
        } else {
            -1
        };

        let pid = process::fork();
        if pid < 0 {
            shell_write(b"fork failed\n");
            for pair in pipes.iter().take(total_pipes) {
                let _ = fs::close_fd_raw(pair[0]);
                let _ = fs::close_fd_raw(pair[1]);
            }
            return 1;
        }
        if pid == 0 {
            run_in_child(
                &pipeline.commands[i],
                tokens,
                stdin_fd,
                stdout_fd,
                &pipes,
                total_pipes,
                pgid,
            );
        }

        let child_pid = pid as u32;
        if pgid == 0 {
            pgid = child_pid;
            let _ = process::setpgid(child_pid, child_pid);
        } else {
            let _ = process::setpgid(child_pid, pgid);
        }
        pids[i] = child_pid;
    }

    for pair in pipes.iter().take(total_pipes) {
        if pair[1] >= 0 {
            let _ = fs::close_fd_raw(pair[1]);
        }
    }
    for pair in pipes.iter().take(inter_pipes) {
        if pair[0] >= 0 {
            let _ = fs::close_fd_raw(pair[0]);
        }
    }

    if pipeline.background {
        let mut cmd_buf = [0u8; 128];
        let cmd_len = command_text(pipeline, tokens, &mut cmd_buf);
        if let Some(job_id) = jobs::add(pgid, pgid, &cmd_buf[..cmd_len]) {
            print_background_job_started(job_id, pgid);
        } else {
            shell_write(b"jobs: table full\n");
        }
        return 0;
    }

    enter_foreground(pgid);

    let capture_fd = pipes[inter_pipes][0];
    if capture_fd >= 0 {
        // Non-blocking read + poll so we keep forwarding
        // compositor keyboard events to the PTY (avoids deadlocking
        // children that read from the TTY slave).
        let _ = crate::syscall::net::set_nonblocking(capture_fd);
        let mut buf = [0u8; 512];
        let mut all_exited = false;
        let mut status = 0;
        let cmd_count = pipeline.command_count;
        let mut reaped = [false; 16];
        loop {
            let mut pfds = [UserPollFd {
                fd: capture_fd,
                events: POLLIN,
                revents: 0,
            }];
            let _ = fs::poll(&mut pfds, 10);

            forward_compositor_keyboard();

            loop {
                match fs::read_slice(capture_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        shell_write(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }

            if !all_exited {
                let mut done = true;
                for (idx, pid) in pids.iter().take(cmd_count).enumerate() {
                    if reaped[idx] {
                        continue;
                    }
                    if let Some(st) = process::waitpid_nohang(*pid) {
                        reaped[idx] = true;
                        if idx == cmd_count - 1 {
                            status = st;
                        }
                    } else {
                        done = false;
                    }
                }
                if done {
                    all_exited = true;
                }
            } else {
                // Children exited -- drain remaining data then stop.
                break;
            }
        }
        // Final drain after children have exited.
        loop {
            match fs::read_slice(capture_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    shell_write(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        let _ = fs::close_fd_raw(capture_fd);
        leave_foreground();
        return status;
    }

    let mut status = 0;
    for pid in pids.iter().take(pipeline.command_count) {
        loop {
            if let Some(st) = process::waitpid_nohang(*pid) {
                status = st;
                break;
            }
            forward_compositor_keyboard();
            sys_core::sleep_ms(5);
        }
    }
    leave_foreground();
    status
}

pub fn execute_tokens(tokens: &ParsedTokens) -> i32 {
    let mut pipeline = ParsedPipeline::empty();
    if parse_pipeline(tokens, &mut pipeline).is_err() {
        shell_write(b"syntax error\n");
        return 1;
    }

    simplify_pipeline(&mut pipeline, tokens);

    if pipeline.command_count == 1 && !pipeline.background {
        let cmd = &pipeline.commands[0];
        if is_builtin_command(cmd, tokens) {
            return execute_single_builtin(cmd, tokens);
        }
        if let Some(status) = execute_registry_spawn(cmd, tokens, false) {
            return status;
        }
    }

    if pipeline.command_count == 1
        && pipeline.background
        && let Some(status) = execute_registry_spawn(&pipeline.commands[0], tokens, true)
    {
        return status;
    }

    for cmd in pipeline.commands.iter().take(pipeline.command_count) {
        if !command_resolves(cmd, tokens) {
            return 127;
        }
    }

    execute_pipeline(&pipeline, tokens)
}
