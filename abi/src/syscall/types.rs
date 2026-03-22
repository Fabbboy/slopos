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

/// Per-task entry returned by SYSCALL_PROCESS_LIST.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTaskEntry {
    pub task_id: u32,
    pub parent_task_id: u32,
    pub process_id: u32,
    pub state: u8,
    pub block_reason: u8,
    pub priority: u8,
    pub last_cpu: u8,
    pub cpu_affinity: u32,
    pub total_runtime_us: u64,
    pub creation_time_ms: u64,
    pub yield_count: u32,
    pub _pad: u32,
    pub name: [u8; 32],
}

/// CPU identification returned by SYSCALL_CPU_INFO.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserCpuInfo {
    pub vendor: [u8; 16],
    pub brand_string: [u8; 48],
    pub cpu_count: u32,
    pub family: u8,
    pub model: u8,
    pub stepping: u8,
    pub _pad: u8,
    pub features: u64,
}

impl Default for UserCpuInfo {
    fn default() -> Self {
        Self {
            vendor: [0u8; 16],
            brand_string: [0u8; 48],
            cpu_count: 0,
            family: 0,
            model: 0,
            stepping: 0,
            _pad: 0,
            features: 0,
        }
    }
}

/// Per-CPU scheduler stats returned by SYSCALL_PERCPU_STATS.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPerCpuStats {
    pub cpu_id: u32,
    pub _pad: u32,
    pub total_switches: u64,
    pub total_ticks: u64,
    pub idle_ticks: u64,
    pub ready_count: u32,
    pub _pad2: u32,
}
