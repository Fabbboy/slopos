use crate::runtime;
use crate::syscall::process;

use super::SyncUnsafeCell;
use super::display::shell_write;

const MAX_JOBS: usize = 16;
const MAX_CMD: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
}

#[derive(Clone, Copy)]
pub struct Job {
    pub active: bool,
    pub job_id: u16,
    pub pid: u32,
    pub pgid: u32,
    pub state: JobState,
    pub command: [u8; MAX_CMD],
    pub command_len: usize,
}

impl Job {
    const fn empty() -> Self {
        Self {
            active: false,
            job_id: 0,
            pid: 0,
            pgid: 0,
            state: JobState::Done,
            command: [0; MAX_CMD],
            command_len: 0,
        }
    }
}

struct JobTable {
    jobs: [Job; MAX_JOBS],
    next_id: u16,
}

impl JobTable {
    const fn new() -> Self {
        Self {
            jobs: [Job::empty(); MAX_JOBS],
            next_id: 1,
        }
    }
}

static JOBS: SyncUnsafeCell<JobTable> = SyncUnsafeCell::new(JobTable::new());

fn with_jobs<R, F: FnOnce(&mut JobTable) -> R>(f: F) -> R {
    f(unsafe { &mut *JOBS.get() })
}

fn next_job_id(table: &mut JobTable) -> u16 {
    let id = table.next_id;
    table.next_id = table.next_id.wrapping_add(1);
    if table.next_id == 0 {
        table.next_id = 1;
    }
    id
}

pub fn add(pid: u32, pgid: u32, command: &[u8]) -> Option<u16> {
    with_jobs(|table| {
        let new_job_id = next_job_id(table);
        for job in &mut table.jobs {
            if !job.active {
                job.active = true;
                job.job_id = new_job_id;
                job.pid = pid;
                job.pgid = pgid;
                job.state = JobState::Running;
                job.command_len = command.len().min(MAX_CMD - 1);
                job.command[..job.command_len].copy_from_slice(&command[..job.command_len]);
                job.command[job.command_len] = 0;
                return Some(job.job_id);
            }
        }
        None
    })
}

pub fn remove_by_job_id(job_id: u16) -> bool {
    with_jobs(|table| {
        for job in &mut table.jobs {
            if job.active && job.job_id == job_id {
                *job = Job::empty();
                return true;
            }
        }
        false
    })
}

pub fn remove_by_pid(pid: u32) -> bool {
    with_jobs(|table| {
        for job in &mut table.jobs {
            if job.active && job.pid == pid {
                *job = Job::empty();
                return true;
            }
        }
        false
    })
}

pub fn mark_done_by_pid(pid: u32) -> bool {
    with_jobs(|table| {
        for job in &mut table.jobs {
            if job.active && job.pid == pid {
                job.state = JobState::Done;
                return true;
            }
        }
        false
    })
}

pub fn find_pid_by_job_id(job_id: u16) -> Option<u32> {
    with_jobs(|table| {
        for job in &table.jobs {
            if job.active && job.job_id == job_id {
                return Some(job.pid);
            }
        }
        None
    })
}

pub fn render_jobs() {
    with_jobs(|table| {
        for job in &table.jobs {
            if !job.active {
                continue;
            }
            shell_write(b"[");
            write_u64(job.job_id as u64);
            shell_write(b"] ");
            match job.state {
                JobState::Running => shell_write(b"Running "),
                JobState::Done => shell_write(b"Done "),
            };
            shell_write(&job.command[..job.command_len]);
            shell_write(b"\n");
        }
    });
}

pub fn refresh_liveness() {
    with_jobs(|table| {
        for job in &mut table.jobs {
            if !job.active {
                continue;
            }

            if process::waitpid_nohang(job.pid).is_some() {
                job.state = JobState::Done;
                continue;
            }

            if process::kill(job.pid, 0) < 0 {
                job.state = JobState::Done;
            }
        }
    });
}

pub fn notify_completed_jobs() {
    refresh_liveness();
    with_jobs(|table| {
        for job in &mut table.jobs {
            if !job.active || job.state != JobState::Done {
                continue;
            }
            shell_write(b"[");
            write_u64(job.job_id as u64);
            shell_write(b"] Done  ");
            shell_write(&job.command[..job.command_len]);
            shell_write(b"\n");
            *job = Job::empty();
        }
    });
}

pub fn find_pgid_by_job_id(job_id: u16) -> Option<u32> {
    with_jobs(|table| {
        for job in &table.jobs {
            if job.active && job.job_id == job_id {
                return Some(job.pgid);
            }
        }
        None
    })
}

pub fn write_u64(value: u64) {
    let text = format!("{}", value);
    shell_write(text.as_bytes());
}

pub fn parse_u32_arg(ptr: *const u8) -> Option<u32> {
    if ptr.is_null() {
        return None;
    }
    let len = runtime::u_strlen(ptr);
    if len == 0 {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let mut v: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?;
        v = v.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}
