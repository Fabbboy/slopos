use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_USER_MODE};

#[derive(Clone, Copy)]
pub struct ProgramSpec {
    pub name: &'static str,
    pub path: &'static str,
    pub priority: u8,
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
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    ProgramSpec {
        name: "shell",
        path: "/bin/shell",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    ProgramSpec {
        name: "compositor",
        path: "/bin/compositor",
        priority: 4,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR,
        desc: "",
        gui: true,
    },
    ProgramSpec {
        name: "roulette",
        path: "/bin/roulette",
        priority: 5,
        flags: TASK_FLAG_USER_MODE | TASK_FLAG_DISPLAY_EXCLUSIVE,
        desc: "Spin the Wheel of Fate",
        gui: true,
    },
    ProgramSpec {
        name: "file_manager",
        path: "/bin/file_manager",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "Browse filesystem",
        gui: true,
    },
    ProgramSpec {
        name: "sysinfo",
        path: "/bin/sysinfo",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "System information panel",
        gui: true,
    },
    ProgramSpec {
        name: "nmap",
        path: "/bin/nmap",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "Scan network for hosts",
        gui: false,
    },
    ProgramSpec {
        name: "ifconfig",
        path: "/bin/ifconfig",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "Show network configuration",
        gui: false,
    },
    ProgramSpec {
        name: "nc",
        path: "/bin/nc",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "Network Swiss army knife",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: "fork_test",
        path: "/bin/fork_test",
        priority: 5,
        flags: TASK_FLAG_USER_MODE,
        desc: "",
        gui: false,
    },
    #[cfg(feature = "testbins")]
    ProgramSpec {
        name: "io_capture_test",
        path: "/bin/io_capture_test",
        priority: 5,
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
