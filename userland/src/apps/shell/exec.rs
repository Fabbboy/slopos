use core::ffi::c_char;
use core::ptr;

use crate::program_registry;
use crate::syscall::{UserFsStat, core as sys_core, fs, process};
use slopos_abi::fs::{O_APPEND, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};

use super::buffers::ParsedTokens;
use super::builtins;
use super::display::{
    shell_clear_output_fd, shell_error, shell_error_named, shell_set_output_fd, shell_write,
};
use super::jobs;
use super::parser::{SHELL_MAX_ARGS, normalize_path};
use std::sync::atomic::{AtomicU32, Ordering};

const MAX_PIPE_CMDS: usize = 8;
const MAX_REDIRECTS: usize = 8;

/// POSIX reserves 2 for the shell's own usage errors, distinct from any status
/// a command could return.
pub const STATUS_SYNTAX_ERROR: i32 = 2;
/// The command was found but could not be executed.
pub const STATUS_CANNOT_EXECUTE: i32 = 126;
pub const STATUS_NOT_FOUND: i32 = 127;

/// Signals a forked job resets to SIG_DFL: the shell catches SIGINT and ignores
/// SIGTTOU/SIGTTIN/SIGTSTP, none of which a launched job should inherit.
const JOB_CONTROL_DEFAULT_SIGNALS: slopos_abi::signal::SigSet = {
    use slopos_abi::signal::{SIGINT, SIGTSTP, SIGTTIN, SIGTTOU, sig_bit};
    sig_bit(SIGINT) | sig_bit(SIGTTOU) | sig_bit(SIGTTIN) | sig_bit(SIGTSTP)
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Input,
    OutputTruncate,
    OutputAppend,
}

/// Where a redirection points: at a path, or at another descriptor (`2>&1`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RedirectTarget {
    /// Index into `ParsedTokens` of the path word.
    Path(usize),
    /// Descriptor to duplicate from.
    Fd(i32),
    /// `>&-` — close the descriptor.
    Close,
}

#[derive(Clone, Copy)]
struct Redirect {
    kind: RedirectKind,
    /// Descriptor being redirected: 0 for `<`, 1 for `>`, or the IO number.
    fd: i32,
    target: RedirectTarget,
}

impl Redirect {
    const fn empty() -> Self {
        Self {
            kind: RedirectKind::Input,
            fd: 0,
            target: RedirectTarget::Path(0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Connector {
    /// `;` — run regardless.
    Seq,
    /// `&&` — run only if the previous pipeline succeeded.
    And,
    /// `||` — run only if the previous pipeline failed.
    Or,
}

#[derive(Clone, Copy)]
struct ParsedCommand {
    argv: [usize; SHELL_MAX_ARGS], // indices into ParsedTokens
    argc: usize,
    /// Leading `NAME=VALUE` words, as indices into `ParsedTokens`.
    assigns: [usize; SHELL_MAX_ARGS],
    assign_count: usize,
    redirects: [Redirect; MAX_REDIRECTS],
    redirect_count: usize,
}

impl ParsedCommand {
    const fn empty() -> Self {
        Self {
            argv: [0; SHELL_MAX_ARGS],
            argc: 0,
            assigns: [0; SHELL_MAX_ARGS],
            assign_count: 0,
            redirects: [Redirect::empty(); MAX_REDIRECTS],
            redirect_count: 0,
        }
    }
}

/// Split `NAME=VALUE` into its halves.  `NAME` must be a shell identifier, so
/// `./a=b` and `-x=1` stay ordinary arguments.
fn split_assignment(word: &[u8]) -> Option<(&[u8], &[u8])> {
    let eq = word.iter().position(|&b| b == b'=')?;
    if eq == 0 {
        return None;
    }
    let name = &word[..eq];
    if !(name[0].is_ascii_alphabetic() || name[0] == b'_') {
        return None;
    }
    if !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        return None;
    }
    Some((name, &word[eq + 1..]))
}

/// A variable's value before an assignment overrode it, so a one-shot
/// `NAME=VALUE cmd` prefix can be undone once `cmd` has run.
type SavedEnv = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn apply_assignments(cmd: &ParsedCommand, tokens: &ParsedTokens, remember: bool) -> SavedEnv {
    let mut saved = SavedEnv::new();
    for i in 0..cmd.assign_count {
        let Some((name, value)) = split_assignment(tokens.token(cmd.assigns[i])) else {
            continue;
        };
        if remember {
            let previous = super::env::get(name).map(|(buf, len)| buf[..len].to_vec());
            saved.push((name.to_vec(), previous));
        }
        super::env::set(name, value);
    }
    saved
}

fn restore_assignments(saved: SavedEnv) {
    for (name, previous) in saved {
        match previous {
            Some(value) => super::env::set(&name, &value),
            None => {
                super::env::unset(&name);
            }
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

/// Claim the terminal and become a session leader — interactive shells only.
///
/// A shell running a script must stay in the process group its parent placed it
/// in: that group is the terminal's foreground group, so a Ctrl+C the user
/// aimed at the pipeline reaches the script *and* the commands it is running.
pub fn initialize_job_control() {
    if !super::is_interactive() {
        return;
    }

    // A successful TIOCGSID means another session already owns this terminal;
    // detaching it would leave them unable to read their own input.
    if fs::tcgetsid(0).is_err() {
        let _ = process::setsid();
        let _ = fs::tiocsctty(0);
    }

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
    if pgid == 0 || !super::is_interactive() {
        return;
    }
    set_foreground_pgid(pgid);
    let _ = fs::tcsetpgrp(0, pgid);
}

pub fn leave_foreground() {
    if !super::is_interactive() {
        return;
    }
    let pgid = shell_pgid();
    if pgid != 0 {
        let _ = fs::tcsetpgrp(0, pgid);
    }
    clear_foreground_pgid();
}

/// Split a redirection operator token into (io number, kind, dup-or-path).
fn classify_redirect(tok: &[u8]) -> Option<(i32, RedirectKind, bool)> {
    if tok == b"&>" {
        // The stderr half of `&>` is attached by the caller once the path opens.
        return Some((1, RedirectKind::OutputTruncate, false));
    }

    let digits = tok.iter().take_while(|b| b.is_ascii_digit()).count();
    let arrow = tok.get(digits)?;
    let dup = tok.get(digits + 1) == Some(&b'&');
    let kind = match arrow {
        b'<' => RedirectKind::Input,
        b'>' if tok.get(digits + 1) == Some(&b'>') => RedirectKind::OutputAppend,
        b'>' => RedirectKind::OutputTruncate,
        _ => return None,
    };

    let fd = if digits == 0 {
        if *arrow == b'<' { 0 } else { 1 }
    } else {
        let mut n = 0i32;
        for &b in &tok[..digits] {
            n = n.saturating_mul(10).saturating_add((b - b'0') as i32);
        }
        n
    };
    Some((fd, kind, dup))
}

/// Parse tokens `[start, end)` — one pipeline of the and-or list — into `out`.
fn parse_pipeline(
    tokens: &ParsedTokens,
    start: usize,
    end: usize,
    out: &mut ParsedPipeline,
) -> Result<(), ()> {
    *out = ParsedPipeline::empty();
    if start >= end {
        return Err(());
    }

    let mut cmd_idx = 0usize;
    let mut token_idx = start;

    while token_idx < end {
        let tok = tokens.token(token_idx);

        if tok == b"&" {
            if token_idx + 1 != end {
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

        if let Some((fd, kind, dup)) = classify_redirect(tok) {
            if token_idx + 1 >= end {
                return Err(());
            }
            let cmd = &mut out.commands[cmd_idx];
            if cmd.redirect_count >= MAX_REDIRECTS {
                return Err(());
            }
            let operand = tokens.token(token_idx + 1);
            let target = if dup {
                match parse_dup_operand(operand) {
                    Some(t) => t,
                    None => return Err(()),
                }
            } else {
                RedirectTarget::Path(token_idx + 1)
            };
            cmd.redirects[cmd.redirect_count] = Redirect { kind, fd, target };
            cmd.redirect_count += 1;

            // `&>path` is shorthand for `>path 2>&1`.
            if tok == b"&>" {
                if cmd.redirect_count >= MAX_REDIRECTS {
                    return Err(());
                }
                cmd.redirects[cmd.redirect_count] = Redirect {
                    kind: RedirectKind::OutputTruncate,
                    fd: 2,
                    target: RedirectTarget::Fd(1),
                };
                cmd.redirect_count += 1;
            }
            token_idx += 2;
            continue;
        }

        let cmd = &mut out.commands[cmd_idx];

        // `NAME=VALUE` counts as an assignment only before the command name, so
        // `env FOO=bar` passes `FOO=bar` through as an argument.
        if cmd.argc == 0 && split_assignment(tok).is_some() {
            if cmd.assign_count >= SHELL_MAX_ARGS {
                return Err(());
            }
            cmd.assigns[cmd.assign_count] = token_idx;
            cmd.assign_count += 1;
            token_idx += 1;
            continue;
        }

        if cmd.argc >= SHELL_MAX_ARGS {
            return Err(());
        }
        cmd.argv[cmd.argc] = token_idx;
        cmd.argc += 1;
        token_idx += 1;
    }

    let last = &out.commands[cmd_idx];
    if last.argc == 0 && last.assign_count == 0 {
        return Err(());
    }

    out.command_count = cmd_idx + 1;
    Ok(())
}

/// The word after a `>&` / `<&`: a descriptor number, or `-` to close.
fn parse_dup_operand(operand: &[u8]) -> Option<RedirectTarget> {
    if operand == b"-" {
        return Some(RedirectTarget::Close);
    }
    if operand.is_empty() || !operand.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n = 0i32;
    for &b in operand {
        n = n.saturating_mul(10).saturating_add((b - b'0') as i32);
    }
    Some(RedirectTarget::Fd(n))
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
    // Job bookkeeping is a message to the user, not program output.
    if !super::is_interactive() {
        return;
    }
    shell_write(b"[");
    jobs::write_u64(job_id as u64);
    shell_write(b"] ");
    jobs::write_u64(pid as u64);
    shell_write(b"\n");
}

/// Build a NUL-terminated `argv` for the exec/spawn ABI boundary.
///
/// The owned strings come back with the pointer array: the kernel reads through
/// those pointers during the syscall, so the bytes must outlive it.
fn build_c_argv(cmd: &ParsedCommand, tokens: &ParsedTokens) -> (Vec<Vec<u8>>, Vec<*const u8>) {
    let owned: Vec<Vec<u8>> = (0..cmd.argc)
        .map(|i| {
            let tok = tokens.token(cmd.argv[i]);
            let mut s = Vec::with_capacity(tok.len() + 1);
            s.extend_from_slice(tok);
            s.push(0);
            s
        })
        .collect();
    let mut ptrs: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(ptr::null());
    (owned, ptrs)
}

/// Build a NUL-terminated `envp` of `KEY=VALUE` strings from the shell's
/// environment.
fn build_c_envp() -> (Vec<Vec<u8>>, Vec<*const u8>) {
    let mut owned: Vec<Vec<u8>> = Vec::new();
    super::env::for_each(|key, value| {
        let mut s = Vec::with_capacity(key.len() + value.len() + 2);
        s.extend_from_slice(key);
        s.push(b'=');
        s.extend_from_slice(value);
        s.push(0);
        owned.push(s);
    });
    let mut ptrs: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(ptr::null());
    (owned, ptrs)
}

fn wait_for(pid: u32) -> i32 {
    loop {
        if let Some(st) = process::waitpid_nohang(pid) {
            return st;
        }
        sys_core::sleep_ms(5);
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

    // A foreground job gets its own pgrp and the kernel-side handoff, so it is
    // the terminal's foreground group before its first fd-0 read.  Only a shell
    // that owns the terminal may ask: the kernel resolves the handoff from the
    // inherited controlling tty, so a script shell would hand the *user's*
    // foreground group to the child with no way to restore it.
    let spawn_flags = if background || !super::is_interactive() {
        spec.flags
    } else {
        spec.flags | slopos_abi::task::TASK_FLAG_NEW_PGRP | slopos_abi::task::TASK_FLAG_FOREGROUND
    };

    let (argv_owned, argv_ptrs) = build_c_argv(cmd, tokens);
    let (envp_owned, envp_ptrs) = build_c_envp();

    // Cloning the shell's own 0/1/2 keeps `isatty(1)` true for the child, so
    // its stdio stays line-buffered and output appears as it is produced.
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_path_with_env(
        spec.path.as_bytes(),
        &argv_ptrs[..cmd.argc],
        &envp_ptrs[..envp_owned.len()],
        spec.priority,
        spawn_flags,
        &actions,
        0,
    );
    drop(argv_owned);
    drop(envp_owned);

    if tid <= 0 {
        shell_error(b"sh: spawn failed\n");
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
            shell_error(b"sh: job table full\n");
        }
        return Some(0);
    }

    // Idempotent: the child already has this pgid via TASK_FLAG_NEW_PGRP.
    if super::is_interactive() {
        let _ = process::setpgid(pid, pid);
    }
    enter_foreground(pid);

    let status = wait_for(pid);
    leave_foreground();
    Some(status)
}

enum RedirectSource {
    Opened(i32),
    Dup(i32),
    Close,
}

fn open_redirect_target(
    redir: Redirect,
    tokens: &ParsedTokens,
    path_buf: &mut [u8; 256],
) -> Result<(i32, RedirectSource), ()> {
    let path_idx = match redir.target {
        RedirectTarget::Fd(src) => return Ok((redir.fd, RedirectSource::Dup(src))),
        RedirectTarget::Close => return Ok((redir.fd, RedirectSource::Close)),
        RedirectTarget::Path(idx) => idx,
    };

    if normalize_path(tokens.token(path_idx), path_buf) != 0 {
        return Err(());
    }
    let path = path_buf.as_ptr() as *const c_char;

    let flags = match redir.kind {
        RedirectKind::Input => O_RDONLY,
        // O_TRUNC rather than unlink-and-recreate: unlinking destroys the file
        // even when the open then fails, and breaks hard links and device nodes.
        RedirectKind::OutputTruncate => O_WRONLY | O_CREAT | O_TRUNC,
        RedirectKind::OutputAppend => O_WRONLY | O_CREAT | O_APPEND,
    };
    let fd = fs::open_path(path, flags).map_err(|_| ())?;
    Ok((redir.fd, RedirectSource::Opened(fd.into_raw())))
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
        let Ok((target_fd, source)) = open_redirect_target(*redir, tokens, &mut path_buf) else {
            shell_error(b"sh: cannot redirect\n");
            return false;
        };

        // A builtin runs in the shell's own process, so redirecting its stdout
        // points `shell_write` at the file rather than moving fd 1 out from
        // under the shell.
        if let (1, RedirectSource::Opened(opened_fd)) = (target_fd, &source) {
            if *output_fd >= 0 {
                let _ = fs::close_fd_raw(*output_fd);
            }
            *output_fd = *opened_fd;
            continue;
        }

        let backup = match fs::dup(target_fd) {
            Ok(fd) => fd.into_raw(),
            // A descriptor that is not open yet has nothing to restore.
            Err(_) => -1,
        };

        let applied = match source {
            RedirectSource::Opened(opened_fd) => {
                let ok = fs::dup2(opened_fd, target_fd).is_ok();
                let _ = fs::close_fd_raw(opened_fd);
                ok
            }
            RedirectSource::Dup(src) => fs::dup2(src, target_fd).is_ok(),
            RedirectSource::Close => fs::close_fd_raw(target_fd).is_ok(),
        };
        if !applied {
            if backup >= 0 {
                let _ = fs::close_fd_raw(backup);
            }
            shell_error(b"sh: cannot redirect\n");
            return false;
        }

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
    foreground: bool,
) -> ! {
    let job_control = super::is_interactive();

    if job_control {
        if pgid == 0 {
            let _ = process::setpgid(0, 0);
        } else {
            let _ = process::setpgid(0, pgid);
        }
    }

    // The child claims the terminal for its own pgrp, racing the parent's
    // `enter_foreground`; both set the same value.  Must precede the sigdefault
    // reset below: SIGTTOU is still inherited-ignored there, so this tcsetpgrp
    // is not denied to a not-yet-foreground child.
    if foreground && job_control {
        let fg_pgid = if pgid == 0 {
            process::getpid() as u32
        } else {
            pgid
        };
        let _ = fs::tcsetpgrp(0, fg_pgid);
    }

    // The reset must be explicit: execve preserves ignored dispositions, and an
    // in-child builtin never execs at all.
    let _ = process::sigdefault(JOB_CONTROL_DEFAULT_SIGNALS);
    super::interrupt::mark_forked_child();

    // The stage runs one command and exits, so the prefix needs no undoing.
    apply_assignments(cmd, tokens, false);

    if stdin_fd >= 0 {
        if fs::dup2(stdin_fd, 0).is_err() {
            let _ = shell_error(b"sh: cannot set up stdin\n");
            sys_core::exit_with_code(1);
        }
    }
    if stdout_fd >= 0 {
        if fs::dup2(stdout_fd, 1).is_err() {
            let _ = shell_error(b"sh: cannot set up stdout\n");
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
            let Ok((target_fd, source)) = open_redirect_target(*redir, tokens, &mut path_buf)
            else {
                let _ = shell_error(b"sh: cannot redirect\n");
                sys_core::exit_with_code(1);
            };
            if let (1, RedirectSource::Opened(opened_fd)) = (target_fd, &source) {
                if builtin_output_fd != 1 {
                    let _ = fs::close_fd_raw(builtin_output_fd);
                }
                builtin_output_fd = *opened_fd;
                continue;
            }
            let applied = match source {
                RedirectSource::Opened(opened_fd) => {
                    let ok = fs::dup2(opened_fd, target_fd).is_ok();
                    let _ = fs::close_fd_raw(opened_fd);
                    ok
                }
                RedirectSource::Dup(src) => fs::dup2(src, target_fd).is_ok(),
                RedirectSource::Close => fs::close_fd_raw(target_fd).is_ok(),
            };
            if !applied {
                let _ = shell_error(b"sh: cannot redirect\n");
                sys_core::exit_with_code(1);
            }
        }

        shell_set_output_fd(builtin_output_fd);
        let argv_slices: Vec<&[u8]> = (0..cmd.argc).map(|i| tokens.token(cmd.argv[i])).collect();
        let code = (entry.func)(cmd.argc as i32, &argv_slices);
        shell_clear_output_fd();
        if builtin_output_fd != 1 {
            let _ = fs::close_fd_raw(builtin_output_fd);
        }
        sys_core::exit_with_code(code);
    }

    let mut path_buf = [0u8; 256];
    for redir in &cmd.redirects[..cmd.redirect_count] {
        let Ok((target_fd, source)) = open_redirect_target(*redir, tokens, &mut path_buf) else {
            let _ = shell_error(b"sh: cannot redirect\n");
            sys_core::exit_with_code(1);
        };
        let applied = match source {
            RedirectSource::Opened(opened_fd) => {
                let ok = fs::dup2(opened_fd, target_fd).is_ok();
                let _ = fs::close_fd_raw(opened_fd);
                ok
            }
            RedirectSource::Dup(src) => fs::dup2(src, target_fd).is_ok(),
            RedirectSource::Close => fs::close_fd_raw(target_fd).is_ok(),
        };
        if !applied {
            let _ = shell_error(b"sh: cannot redirect\n");
            sys_core::exit_with_code(1);
        }
    }

    let name = tokens.token(cmd.argv[0]);
    if !resolve_exec_path(name, &mut path_buf) {
        shell_error_named(name, b"not found");
        sys_core::exit_with_code(STATUS_NOT_FOUND);
    }

    let (argv_owned, argv_ptrs) = build_c_argv(cmd, tokens);
    let (envp_owned, envp_ptrs) = build_c_envp();

    // POSIX gives an unexecutable image its own status, distinct from a typo.
    let rc = process::execve(path_buf.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    drop(argv_owned);
    drop(envp_owned);
    if rc < 0 {
        shell_error_named(name, b"cannot execute");
    }
    sys_core::exit_with_code(STATUS_CANNOT_EXECUTE);
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
        let argv_slices: Vec<&[u8]> = (0..cmd.argc).map(|i| tokens.token(cmd.argv[i])).collect();
        let rc = (entry.func)(cmd.argc as i32, &argv_slices);
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
    // The last stage inherits the shell's own stdout, so no pipe is made for it
    // and its output reaches the terminal directly.
    let inter_pipes = pipeline.command_count.saturating_sub(1);
    let total_pipes = inter_pipes;
    let mut pipes = [[-1; 2]; MAX_PIPE_CMDS];
    for pair in pipes.iter_mut().take(total_pipes) {
        match fs::pipe() {
            Ok((r, w)) => {
                pair[0] = r.into_raw();
                pair[1] = w.into_raw();
            }
            Err(_) => {
                shell_error(b"sh: cannot create pipe\n");
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
        let stdout_fd = if i < inter_pipes { pipes[i][1] } else { -1 };

        let pid = process::fork();
        if pid < 0 {
            shell_error(b"sh: cannot fork\n");
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
                !pipeline.background,
            );
        }

        let child_pid = pid as u32;
        if pgid == 0 {
            pgid = child_pid;
            if super::is_interactive() {
                let _ = process::setpgid(child_pid, child_pid);
            }
        } else if super::is_interactive() {
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
            shell_error(b"sh: job table full\n");
        }
        return 0;
    }

    enter_foreground(pgid);

    // Every stage is reaped, but the pipeline's status is the last stage's.
    let mut status = 0;
    for (idx, pid) in pids.iter().take(pipeline.command_count).enumerate() {
        let st = wait_for(*pid);
        if idx == pipeline.command_count - 1 {
            status = st;
        }
    }
    leave_foreground();
    status
}

/// Run one line: an and-or list of pipelines joined by `;`, `&&` and `||`.
///
/// Joining is left-associative, as POSIX specifies: `a && b || c` is
/// `(a && b) || c`, and a skipped pipeline leaves the status alone for the next
/// connector to judge.
pub fn execute_tokens(tokens: &ParsedTokens) -> i32 {
    super::interrupt::clear();

    let count = tokens.count();
    if count == 0 {
        return 0;
    }

    let mut status = super::last_exit_code();
    let mut skip = false;
    let mut idx = 0usize;

    while idx < count {
        let (end, connector, next) = split_at_connector(tokens, idx);
        if end == idx {
            shell_error(b"sh: syntax error\n");
            return STATUS_SYNTAX_ERROR;
        }

        if !skip {
            status = execute_and_or_term(tokens, idx, end);
            super::set_last_exit_code(status);
            if super::exit_requested().is_some() {
                return status;
            }
        }

        skip = match connector {
            Connector::Seq => false,
            Connector::And => status != 0,
            Connector::Or => status == 0,
        };
        idx = next;
    }

    status
}

/// Returns the exclusive end of the pipeline's tokens, the connector that
/// follows it, and the index the next pipeline starts at.
fn split_at_connector(tokens: &ParsedTokens, start: usize) -> (usize, Connector, usize) {
    let count = tokens.count();
    let mut idx = start;
    while idx < count {
        let connector = match tokens.token(idx) {
            b";" => Connector::Seq,
            b"&&" => Connector::And,
            b"||" => Connector::Or,
            _ => {
                idx += 1;
                continue;
            }
        };
        return (idx, connector, idx + 1);
    }
    // A trailing pipeline with no connector after it: `;` is the identity.
    (count, Connector::Seq, count)
}

fn execute_and_or_term(tokens: &ParsedTokens, start: usize, end: usize) -> i32 {
    let mut pipeline = ParsedPipeline::empty();
    if parse_pipeline(tokens, start, end, &mut pipeline).is_err() {
        shell_error(b"sh: syntax error\n");
        return STATUS_SYNTAX_ERROR;
    }

    simplify_pipeline(&mut pipeline, tokens);

    // `NAME=VALUE` with no command sets the variable outright, not for the
    // duration of something.
    if pipeline.command_count == 1 && pipeline.commands[0].argc == 0 {
        apply_assignments(&pipeline.commands[0], tokens, false);
        return 0;
    }

    if pipeline.command_count == 1 && !pipeline.background {
        let cmd = &pipeline.commands[0];
        // A prefix belongs to that command alone, so it is put back after.
        let saved = apply_assignments(cmd, tokens, true);
        if is_builtin_command(cmd, tokens) {
            let status = execute_single_builtin(cmd, tokens);
            restore_assignments(saved);
            return status;
        }
        if let Some(status) = execute_registry_spawn(cmd, tokens, false) {
            restore_assignments(saved);
            return status;
        }
        restore_assignments(saved);
    }

    if pipeline.command_count == 1
        && pipeline.background
        && let Some(status) = execute_registry_spawn(&pipeline.commands[0], tokens, true)
    {
        return status;
    }

    for cmd in pipeline.commands.iter().take(pipeline.command_count) {
        if !command_resolves(cmd, tokens) {
            match command_name_bytes(cmd, tokens) {
                Some(name) => shell_error_named(name, b"not found"),
                // A stage with no command word at all — `FOO=bar | cat`.
                None => {
                    shell_error(b"sh: syntax error\n");
                }
            }
            return STATUS_NOT_FOUND;
        }
    }

    execute_pipeline(&pipeline, tokens)
}
