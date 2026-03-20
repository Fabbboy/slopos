#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPollFd {
    pub fd: i32,
    pub events: u16,
    pub revents: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}
// =============================================================================
// Syscall data structures
// =============================================================================

/// System information returned by SYSCALL_SYS_INFO
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
    pub wl_balance: i64,
    pub boot_flags: u32,
}

pub const BOOT_FLAG_ROULETTE_SKIP: u32 = 1 << 0;

/// POSIX-style timespec returned by `SYSCALL_CLOCK_GETTIME`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct Timespec {
    pub tv_sec: u64,
    pub tv_nsec: u64,
}
