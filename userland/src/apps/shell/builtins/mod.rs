//! Builtin command dispatch table and helpers.

pub mod env;
pub mod fs;
pub mod process;
pub mod system;
pub mod utils;

pub type BuiltinFn = fn(argc: i32, argv: &[&[u8]]) -> i32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCategory {
    System,
    Filesystem,
    Process,
    Environment,
    Network,
    Utility,
}

impl BuiltinCategory {
    pub const ALL: &[BuiltinCategory] = &[
        BuiltinCategory::System,
        BuiltinCategory::Filesystem,
        BuiltinCategory::Process,
        BuiltinCategory::Environment,
        BuiltinCategory::Network,
        BuiltinCategory::Utility,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BuiltinCategory::System => "System",
            BuiltinCategory::Filesystem => "Filesystem",
            BuiltinCategory::Process => "Process Control",
            BuiltinCategory::Environment => "Environment",
            BuiltinCategory::Network => "Network",
            BuiltinCategory::Utility => "Utility",
        }
    }
}

pub struct BuiltinEntry {
    pub name: &'static str,
    pub desc: &'static str,
    pub usage: &'static str,
    pub detail: &'static str,
    pub category: BuiltinCategory,
    pub func: BuiltinFn,
}

use BuiltinCategory::*;

pub static BUILTINS: &[BuiltinEntry] = &[
    // ── System ──────────────────────────────────────────────────────────────
    BuiltinEntry {
        name: "help",
        desc: "Show this help",
        usage: "help [command]",
        detail: "Display a categorized list of all shell builtins.\nProvide a command name to see its detailed usage.",
        category: System,
        func: system::cmd_help,
    },
    BuiltinEntry {
        name: "echo",
        desc: "Print arguments to terminal",
        usage: "echo [args...]",
        detail: "Write each argument to standard output separated by\nspaces, followed by a newline.",
        category: System,
        func: system::cmd_echo,
    },
    BuiltinEntry {
        name: "clear",
        desc: "Clear the screen",
        usage: "clear",
        detail: "Reset the terminal display and move the cursor to\nthe top-left corner.",
        category: System,
        func: system::cmd_clear,
    },
    BuiltinEntry {
        name: "info",
        desc: "Kernel and scheduler stats",
        usage: "info",
        detail: "Print memory page counts, active tasks, context\nswitches, and scheduler statistics.",
        category: System,
        func: system::cmd_info,
    },
    BuiltinEntry {
        name: "shutdown",
        desc: "Power off the system",
        usage: "shutdown",
        detail: "Immediately halt the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_shutdown,
    },
    BuiltinEntry {
        name: "reboot",
        desc: "Reboot the system",
        usage: "reboot",
        detail: "Immediately restart the machine. All unsaved state\nwill be lost.",
        category: System,
        func: system::cmd_reboot,
    },
    BuiltinEntry {
        name: "uptime",
        desc: "Show system uptime",
        usage: "uptime",
        detail: "Display time elapsed since boot in hours, minutes,\nand seconds, plus total milliseconds.",
        category: System,
        func: system::cmd_uptime,
    },
    BuiltinEntry {
        name: "cpuinfo",
        desc: "Show CPU information",
        usage: "cpuinfo",
        detail: "Display architecture, CPU count, and which CPU the\nshell is currently running on.",
        category: System,
        func: system::cmd_cpuinfo,
    },
    BuiltinEntry {
        name: "free",
        desc: "Show memory usage",
        usage: "free",
        detail: "Display memory statistics in pages, KiB, and MiB.\nShows total, free, and allocated memory.",
        category: System,
        func: system::cmd_free,
    },
    BuiltinEntry {
        name: "time",
        desc: "Time a command",
        usage: "time <command> [args...]",
        detail: "Execute a command and report wall-clock elapsed\ntime after it completes.",
        category: System,
        func: system::cmd_time,
    },
    BuiltinEntry {
        name: "date",
        desc: "Show current time",
        usage: "date",
        detail: "Display the current time relative to boot. SlopOS\nhas no real-time clock, so time is measured from\nsystem start in Sloptopia Local Time (SLT).",
        category: System,
        func: system::cmd_date,
    },
    BuiltinEntry {
        name: "uname",
        desc: "System identification",
        usage: "uname [-a] [-s] [-r] [-m]",
        detail: "Print system information. Flags:\n  -s  System name (SlopOS)\n  -r  Release (0.2-slop)\n  -m  Machine (x86_64)\n  -a  All of the above (default)",
        category: System,
        func: system::cmd_uname,
    },
    BuiltinEntry {
        name: "whoami",
        desc: "Print current user",
        usage: "whoami",
        detail: "Print the effective user name. SlopOS always runs\nas root (uid 0).",
        category: System,
        func: system::cmd_whoami,
    },
    // ── Filesystem ──────────────────────────────────────────────────────────
    BuiltinEntry {
        name: "ls",
        desc: "List directory contents",
        usage: "ls [path]",
        detail: "List files and directories at the given path.\nDirectories are marked with /, files show name (size).\nEntries are sorted alphabetically. Defaults to cwd.",
        category: Filesystem,
        func: fs::cmd_ls,
    },
    BuiltinEntry {
        name: "cat",
        desc: "Display file contents",
        usage: "cat [file...]",
        detail: "Print the contents of one or more files to the\nterminal. Without arguments, reads from stdin.\nEach file is truncated at 512 bytes.",
        category: Filesystem,
        func: fs::cmd_cat,
    },
    BuiltinEntry {
        name: "write",
        desc: "Write text to a file",
        usage: "write <file> <text>",
        detail: "Create or overwrite a file with the given text.\nThe previous contents are replaced entirely.",
        category: Filesystem,
        func: fs::cmd_write,
    },
    BuiltinEntry {
        name: "mkdir",
        desc: "Create a directory",
        usage: "mkdir <dir>",
        detail: "Create a new directory at the given path.",
        category: Filesystem,
        func: fs::cmd_mkdir,
    },
    BuiltinEntry {
        name: "rm",
        desc: "Remove a file",
        usage: "rm <file>",
        detail: "Delete a file. Does not remove directories.",
        category: Filesystem,
        func: fs::cmd_rm,
    },
    BuiltinEntry {
        name: "cd",
        desc: "Change working directory",
        usage: "cd [dir]",
        detail: "Change the current working directory to dir.\nWithout arguments, returns to /.\nUse cd .. to go up one level.",
        category: Filesystem,
        func: fs::cmd_cd,
    },
    BuiltinEntry {
        name: "pwd",
        desc: "Print working directory",
        usage: "pwd",
        detail: "Print the absolute path of the current working\ndirectory.",
        category: Filesystem,
        func: fs::cmd_pwd,
    },
    BuiltinEntry {
        name: "stat",
        desc: "Show file information",
        usage: "stat <path>",
        detail: "Display file type and size for the given path.",
        category: Filesystem,
        func: fs::cmd_stat,
    },
    BuiltinEntry {
        name: "touch",
        desc: "Create empty file",
        usage: "touch <path...>",
        detail: "Create an empty file at each given path. If the\nfile already exists, it is left unchanged.",
        category: Filesystem,
        func: fs::cmd_touch,
    },
    BuiltinEntry {
        name: "cp",
        desc: "Copy a file",
        usage: "cp <src> <dst>",
        detail: "Copy the contents of src to dst. Overwrites dst\nif it exists. Does not copy directories.",
        category: Filesystem,
        func: fs::cmd_cp,
    },
    BuiltinEntry {
        name: "mv",
        desc: "Move a file",
        usage: "mv <src> <dst>",
        detail: "Move src to dst (copy then remove). Overwrites\ndst if it exists. Does not move directories.",
        category: Filesystem,
        func: fs::cmd_mv,
    },
    BuiltinEntry {
        name: "head",
        desc: "Show first lines of file",
        usage: "head <file> [n]",
        detail: "Print the first N lines of a file (default 10).",
        category: Filesystem,
        func: fs::cmd_head,
    },
    BuiltinEntry {
        name: "tail",
        desc: "Show last lines of file",
        usage: "tail <file> [n]",
        detail: "Print the last N lines of a file (default 10).\nBuffers up to 4096 bytes from the file.",
        category: Filesystem,
        func: fs::cmd_tail,
    },
    BuiltinEntry {
        name: "wc",
        desc: "Count lines, words, chars",
        usage: "wc [file...]",
        detail: "Count lines, words, and characters in each file.\nWithout arguments, reads from standard input.\nWith multiple files, prints a total line.",
        category: Filesystem,
        func: fs::cmd_wc,
    },
    BuiltinEntry {
        name: "hexdump",
        desc: "Hex and ASCII dump",
        usage: "hexdump <file> [n]",
        detail: "Display the first N bytes of a file in hexadecimal\nand ASCII (default 256, max 512).",
        category: Filesystem,
        func: fs::cmd_hexdump,
    },
    BuiltinEntry {
        name: "diff",
        desc: "Compare two files",
        usage: "diff <file1> <file2>",
        detail: "Compare two files line by line. Show differing\nlines with < and > markers. Returns 0 if files\nare identical, 1 if they differ.",
        category: Filesystem,
        func: fs::cmd_diff,
    },
    BuiltinEntry {
        name: "tee",
        desc: "Copy stdin to stdout and file",
        usage: "tee [-a] [file]",
        detail: "Read from standard input and write to both stdout\nand a file. Use -a to append instead of overwrite.\nWithout a file argument, passes stdin through.",
        category: Filesystem,
        func: fs::cmd_tee,
    },
    // ── Process Control ─────────────────────────────────────────────────────
    BuiltinEntry {
        name: "jobs",
        desc: "List background jobs",
        usage: "jobs",
        detail: "Show all active background jobs with their job\nnumber, process ID, and current status.",
        category: Process,
        func: process::cmd_jobs,
    },
    BuiltinEntry {
        name: "fg",
        desc: "Bring job to foreground",
        usage: "fg <%job>",
        detail: "Resume a stopped or background job in the\nforeground. Specify the job with %N notation\n(e.g. fg %1).",
        category: Process,
        func: process::cmd_fg,
    },
    BuiltinEntry {
        name: "bg",
        desc: "Resume a stopped job",
        usage: "bg <%job>",
        detail: "Continue a stopped job in the background.\nSpecify the job with %N notation (e.g. bg %1).",
        category: Process,
        func: process::cmd_bg,
    },
    BuiltinEntry {
        name: "kill",
        desc: "Send signal to process",
        usage: "kill <pid | %job>",
        detail: "Send SIGKILL to a process by PID or to a job\ngroup by %N notation (e.g. kill %1 or kill 42).",
        category: Process,
        func: process::cmd_kill,
    },
    BuiltinEntry {
        name: "ps",
        desc: "Show running processes",
        usage: "ps",
        detail: "Display task counts (total, active, ready) and\nlist windowed processes with their PID, state,\nand title.",
        category: Process,
        func: process::cmd_ps,
    },
    BuiltinEntry {
        name: "wait",
        desc: "Wait for process to exit",
        usage: "wait <pid>",
        detail: "Block the shell until the process with the given\nPID exits. Returns that process's exit status.",
        category: Process,
        func: process::cmd_wait,
    },
    BuiltinEntry {
        name: "exec",
        desc: "Replace shell with program",
        usage: "exec <path>",
        detail: "Replace the current shell process with the program\nat the given path. Does not return on success.",
        category: Process,
        func: process::cmd_exec,
    },
    // ── Environment ─────────────────────────────────────────────────────────
    BuiltinEntry {
        name: "export",
        desc: "Set environment variable",
        usage: "export [KEY=VALUE...]",
        detail: "Set one or more environment variables.\nWithout arguments, print all exported variables.",
        category: Environment,
        func: env::cmd_export,
    },
    BuiltinEntry {
        name: "unset",
        desc: "Remove environment variable",
        usage: "unset <KEY...>",
        detail: "Remove one or more variables from the environment.",
        category: Environment,
        func: env::cmd_unset,
    },
    BuiltinEntry {
        name: "env",
        desc: "List environment variables",
        usage: "env",
        detail: "Print all environment variables in KEY=VALUE format.",
        category: Environment,
        func: env::cmd_env,
    },
    BuiltinEntry {
        name: "set",
        desc: "Show or set shell variables",
        usage: "set [KEY=VALUE...]",
        detail: "Set shell variables or, without arguments, list\nall current variables.",
        category: Environment,
        func: env::cmd_set,
    },
    // ── Utility ─────────────────────────────────────────────────────────────
    BuiltinEntry {
        name: "sleep",
        desc: "Sleep for N milliseconds",
        usage: "sleep <ms>",
        detail: "Pause execution for the specified number of\nmilliseconds.",
        category: Utility,
        func: utils::cmd_sleep,
    },
    BuiltinEntry {
        name: "true",
        desc: "Return success",
        usage: "true",
        detail: "Do nothing and return exit code 0.",
        category: Utility,
        func: utils::cmd_true,
    },
    BuiltinEntry {
        name: "false",
        desc: "Return failure",
        usage: "false",
        detail: "Do nothing and return exit code 1.",
        category: Utility,
        func: utils::cmd_false,
    },
    BuiltinEntry {
        name: "seq",
        desc: "Print number sequence",
        usage: "seq [start] <end>",
        detail: "Print integers from start to end, one per line.\nIf only one argument is given, start defaults to 1.",
        category: Utility,
        func: utils::cmd_seq,
    },
    BuiltinEntry {
        name: "yes",
        desc: "Repeat a string",
        usage: "yes [string]",
        detail: "Repeatedly print a string (default 'y') followed\nby a newline, until interrupted with Ctrl+C.\nUseful in pipelines (yes | head -5).",
        category: Utility,
        func: utils::cmd_yes,
    },
    BuiltinEntry {
        name: "random",
        desc: "Print a random number",
        usage: "random [max]",
        detail: "Print a random number. With max, prints a value\nin the range 0..max (exclusive). Without max,\nprints a raw 32-bit random value.",
        category: Utility,
        func: utils::cmd_random,
    },
    BuiltinEntry {
        name: "roulette",
        desc: "Spin the Wheel of Fate",
        usage: "roulette",
        detail: "Gamble with destiny. A win awards +10 W's.\nA loss reboots the system. The house always wins.\nEventually.",
        category: Utility,
        func: utils::cmd_roulette,
    },
    BuiltinEntry {
        name: "wl",
        desc: "Show W/L balance",
        usage: "wl",
        detail: "Display the current W/L currency balance from\nthe Wheel of Fate's ledger.",
        category: Utility,
        func: utils::cmd_wl,
    },
    // ── Network ────────────────────────────────────────────────────────────
    BuiltinEntry {
        name: "resolve",
        desc: "Resolve hostname to IP",
        usage: "resolve <hostname>",
        detail: "Resolve a hostname to its IPv4 address using the\nin-kernel DNS client. Uses the DHCP-provided DNS\nserver (typically 10.0.2.3 on QEMU user-net).",
        category: Network,
        func: system::cmd_resolve,
    },
];

pub fn find_builtin(name: &[u8]) -> Option<&'static BuiltinEntry> {
    for entry in BUILTINS {
        if name == entry.name.as_bytes() {
            return Some(entry);
        }
    }
    None
}
