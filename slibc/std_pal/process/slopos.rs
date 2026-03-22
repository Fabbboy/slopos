#![deny(unsafe_op_in_unsafe_fn)]

use super::env::{CommandEnv, CommandEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::pipe::Pipe;
use crate::{fmt, io};

use crate::io::Read;

unsafe extern "C" {
    fn fork() -> i32;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(status: i32) -> !;
    fn slopos_pipe(fds: *mut i32) -> i32;
    fn close(fd: i32) -> i32;
    fn slopos_dup2(old: i32, new: i32) -> i32;
    fn slopos_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "getpid"]
    fn libc_getpid() -> i32;
}

pub fn getpid() -> u32 {
    unsafe { libc_getpid() as u32 }
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

pub type ChildPipe = crate::sys::pipe::Pipe;

#[derive(Debug)]
#[allow(dead_code)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    InheritFile(crate::sys::fs::File),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExitStatus(i32);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExitStatusError(NonZero<i32>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(u8);

#[derive(Debug, Clone, Copy)]
pub struct Process {
    pid: i32,
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        let program = program.to_os_string();
        Command {
            args: vec![program.clone()],
            program,
            env: CommandEnv::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_os_string());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &Path) {
        self.cwd = Some(dir.as_os_str().to_os_string());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        CommandArgs {
            iter: self.args[1..].iter(),
        }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.is_unchanged()
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(Path::new)
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        _needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let stdin_cfg = self.stdin.as_ref().unwrap_or(&default);
        let stdout_cfg = self.stdout.as_ref().unwrap_or(&default);
        let stderr_cfg = self.stderr.as_ref().unwrap_or(&default);

        let mut stdin_pipe = None;
        let mut stdout_pipe = None;
        let mut stderr_pipe = None;

        if matches!(stdin_cfg, Stdio::MakePipe) {
            stdin_pipe = Some(create_pipe()?);
        }
        if matches!(stdout_cfg, Stdio::MakePipe) {
            stdout_pipe = Some(create_pipe()?);
        }
        if matches!(stderr_cfg, Stdio::MakePipe) {
            stderr_pipe = Some(create_pipe()?);
        }

        let pid = unsafe { fork() };
        if pid < 0 {
            close_pipe_pair(stdin_pipe);
            close_pipe_pair(stdout_pipe);
            close_pipe_pair(stderr_pipe);
            return Err(errno_from_ret(pid));
        }

        if pid == 0 {
            child_setup_stdio(0, stdin_cfg, stdin_pipe);
            child_setup_stdio(1, stdout_cfg, stdout_pipe);
            child_setup_stdio(2, stderr_cfg, stderr_pipe);

            let program = osstr_to_cstring_bytes(self.program.as_os_str());
            let argv_store: Vec<Vec<u8>> = self
                .args
                .iter()
                .map(|a| osstr_to_cstring_bytes(a.as_os_str()))
                .collect();
            let mut argv: Vec<*const u8> = argv_store.iter().map(|s| s.as_ptr()).collect();
            argv.push(crate::ptr::null());

            let mut env_store: Vec<Vec<u8>> = Vec::new();
            for (k, v) in self.get_envs() {
                if let Some(v) = v {
                    let mut item = Vec::new();
                    item.extend_from_slice(k.as_os_str().as_encoded_bytes());
                    item.push(b'=');
                    item.extend_from_slice(v.as_os_str().as_encoded_bytes());
                    item.push(0);
                    env_store.push(item);
                }
            }
            let mut envp: Vec<*const u8> = env_store.iter().map(|s| s.as_ptr()).collect();
            envp.push(crate::ptr::null());

            let rc = unsafe { execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
            let code = if rc < 0 { 127 } else { rc };
            unsafe { _exit(code) }
        }

        let mut pipes = StdioPipes {
            stdin: None,
            stdout: None,
            stderr: None,
        };

        if let Some((read_end, write_end)) = stdin_pipe {
            unsafe {
                let _ = close(read_end);
            }
            pipes.stdin = Some(unsafe { Pipe::from_raw_fd(write_end) });
        }

        if let Some((read_end, write_end)) = stdout_pipe {
            unsafe {
                let _ = close(write_end);
            }
            pipes.stdout = Some(unsafe { Pipe::from_raw_fd(read_end) });
        }

        if let Some((read_end, write_end)) = stderr_pipe {
            unsafe {
                let _ = close(write_end);
            }
            pipes.stderr = Some(unsafe { Pipe::from_raw_fd(read_end) });
        }

        Ok((Process { pid }, pipes))
    }
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(OsString::as_os_str)
    }
}

impl Stdio {
    fn fd_for_child(&self, target: i32) -> Option<i32> {
        match self {
            Stdio::Inherit => None,
            Stdio::Null => Some(-1),
            Stdio::MakePipe => None,
            Stdio::ParentStdout => Some(1),
            Stdio::ParentStderr => Some(2),
            Stdio::InheritFile(file) => Some(file.as_raw_fd()),
        }
    }
}

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.exited() && self.code() == Some(0) {
            Ok(())
        } else {
            let status = if self.0 == 0 { 1 } else { self.0 };
            let nonzero = NonZero::new(status).expect("status must be non-zero");
            Err(ExitStatusError(nonzero))
        }
    }

    pub fn code(&self) -> Option<i32> {
        if self.exited() {
            Some((self.0 >> 8) & 0xff)
        } else {
            None
        }
    }

    fn exited(&self) -> bool {
        (self.0 & 0x7f) == 0
    }

    fn signaled(&self) -> bool {
        (((self.0 & 0x7f) + 1) >> 1) > 0
    }

    fn signal(&self) -> i32 {
        self.0 & 0x7f
    }
}

impl ExitStatusError {
    pub fn code(&self) -> Option<NonZero<i32>> {
        NonZero::new((self.0.get() >> 8) & 0xff)
    }
}

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get())
    }
}

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(value: u8) -> Self {
        ExitCode(value)
    }
}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let rc = unsafe { slopos_kill(self.pid, 9) };
        if rc < 0 {
            Err(errno_from_ret(rc))
        } else {
            Ok(())
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut status = 0;
        let rc = unsafe { waitpid(self.pid, &mut status as *mut i32, 0) };
        if rc < 0 {
            Err(errno_from_ret(rc))
        } else {
            Ok(ExitStatus(status))
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut status = 0;
        let rc = unsafe { waitpid(self.pid, &mut status as *mut i32, 1) };
        if rc < 0 {
            Err(errno_from_ret(rc))
        } else if rc == 0 {
            Ok(None)
        } else {
            Ok(Some(ExitStatus(status)))
        }
    }
}

pub fn output(_cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Command::output is not supported on SlopOS yet",
    ))
}

pub fn read_output(
    mut out: ChildPipe,
    stdout: &mut Vec<u8>,
    mut err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .finish()
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exited() {
            write!(f, "exit status: {}", (self.0 >> 8) & 0xff)
        } else if self.signaled() {
            write!(f, "signal: {}", self.signal())
        } else {
            write!(f, "exit status: {}", self.0)
        }
    }
}

impl fmt::Debug for ExitStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExitStatusError").field(&self.0).finish()
    }
}

impl From<ChildPipe> for Stdio {
    fn from(_pipe: ChildPipe) -> Self {
        Stdio::MakePipe
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Self {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Self {
        Stdio::ParentStderr
    }
}

impl From<crate::sys::fs::File> for Stdio {
    fn from(file: crate::sys::fs::File) -> Self {
        Stdio::InheritFile(file)
    }
}

fn osstr_to_cstring_bytes(s: &OsStr) -> Vec<u8> {
    let mut bytes = s.as_encoded_bytes().to_vec();
    if bytes.last().copied() != Some(0) {
        bytes.push(0);
    }
    bytes
}

fn errno_from_ret(ret: i32) -> io::Error {
    io::Error::from_raw_os_error(-ret)
}

fn create_pipe() -> io::Result<(i32, i32)> {
    let mut fds = [0_i32; 2];
    let rc = unsafe { slopos_pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        Err(errno_from_ret(rc))
    } else {
        Ok((fds[0], fds[1]))
    }
}

fn close_pipe_pair(pipe: Option<(i32, i32)>) {
    if let Some((a, b)) = pipe {
        unsafe {
            let _ = close(a);
            let _ = close(b);
        }
    }
}

fn child_setup_stdio(target_fd: i32, stdio: &Stdio, pipe: Option<(i32, i32)>) {
    match (stdio, pipe) {
        (Stdio::MakePipe, Some((read_end, write_end))) => {
            let chosen = if target_fd == 0 { read_end } else { write_end };
            let other = if target_fd == 0 { write_end } else { read_end };
            unsafe {
                let _ = slopos_dup2(chosen, target_fd);
                let _ = close(chosen);
                let _ = close(other);
            }
        }
        (_, Some((read_end, write_end))) => unsafe {
            let _ = close(read_end);
            let _ = close(write_end);
        },
        _ => {
            if let Some(fd) = stdio.fd_for_child(target_fd) {
                if fd >= 0 {
                    unsafe {
                        let _ = slopos_dup2(fd, target_fd);
                    }
                } else {
                    unsafe {
                        let _ = close(target_fd);
                    }
                }
            }
        }
    }
}
