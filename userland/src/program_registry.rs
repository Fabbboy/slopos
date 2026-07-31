use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};

#[derive(Clone, Copy)]
pub struct ProgramSpec {
    pub name: &'static str,
    pub path: &'static str,
    /// The tier this program is *requested* at. User space may name only
    /// `Normal` and `Low`; anything else is `EINVAL` at the spawn boundary. A
    /// program that needs a higher tier is given it kernel-side by program
    /// identity — the compositor runs at `High` despite this field saying
    /// `Normal`.
    pub priority: TaskPriority,
    /// The *unprivileged* flags this program is spawned with. Privileged bits
    /// (`SYSTEM`, `COMPOSITOR`, `DISPLAY_EXCLUSIVE`, `NO_PREEMPT`) are refused
    /// with `EPERM` if named here; the kernel confers them by program identity
    /// instead.
    pub flags: u16,
    pub desc: &'static str,
    /// If true, the program owns a display surface and should be spawned
    /// directly via `spawn_path_with_attrs`. Text programs (gui=false) fall
    /// through to the fork+execve pipeline so stdout is properly captured.
    pub gui: bool,
}

const PROGRAM_REGISTRY: &[ProgramSpec] = &[
    ProgramSpec {
        name: "init",
        path: "/sbin/init",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    ProgramSpec {
        name: "shell",
        path: "/bin/shell",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    ProgramSpec {
        name: "compositor",
        path: "/bin/compositor",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: true,
    },
    ProgramSpec {
        name: "terminal",
        path: "/bin/terminal",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: true,
    },
    ProgramSpec {
        name: "roulette",
        path: "/bin/roulette",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "Spin the Wheel of Fate",
        gui: true,
    },
    ProgramSpec {
        name: "file_manager",
        path: "/bin/file_manager",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "Browse filesystem",
        gui: true,
    },
    ProgramSpec {
        name: "image_viewer",
        path: "/bin/image_viewer",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "View PNG images",
        gui: true,
    },
    ProgramSpec {
        name: "sysmon",
        path: "/bin/sysmon",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "System Monitor",
        gui: true,
    },
    ProgramSpec {
        name: "nmap",
        path: "/bin/nmap",
        priority: TaskPriority::Low,
        flags: TASK_FLAG_USER_MODE,
        desc: "Scan network for hosts",
        gui: false,
    },
    ProgramSpec {
        name: "ifconfig",
        path: "/bin/ifconfig",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "Show network configuration",
        gui: false,
    },
    ProgramSpec {
        name: "nc",
        path: "/bin/nc",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "Network Swiss army knife",
        gui: false,
    },
    ProgramSpec {
        name: "curl",
        path: "/bin/curl",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "Transfer data from URLs",
        gui: false,
    },
    ProgramSpec {
        name: "ping",
        path: "/bin/ping",
        priority: TaskPriority::Low,
        flags: TASK_FLAG_USER_MODE,
        desc: "Send ICMP ECHO_REQUEST to network hosts",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: "fork_test",
        path: "/bin/fork_test",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: "io_capture_test",
        path: "/bin/io_capture_test",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: "heap_allocator_test",
        path: "/bin/heap_allocator_test",
        priority: TaskPriority::Normal,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
];

fn trim_nul_bytes(text: &str) -> &str {
    text.split('\0').next().unwrap_or(text)
}

pub fn resolve_program(name: &str) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(name);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.name) == requested)
}

pub fn resolve_program_path(path: &str) -> Option<&'static ProgramSpec> {
    let requested = trim_nul_bytes(path);
    PROGRAM_REGISTRY
        .iter()
        .find(|spec| trim_nul_bytes(spec.path) == requested)
}

pub fn user_programs() -> impl Iterator<Item = &'static ProgramSpec> {
    PROGRAM_REGISTRY.iter().filter(|spec| !spec.desc.is_empty())
}
