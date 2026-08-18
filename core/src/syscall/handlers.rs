use core::ffi::c_char;

use slopos_abi::syscall::*;

use crate::syscall::common::SyscallEntry;
pub use crate::syscall::core_handlers::{
    syscall_clock_gettime, syscall_cpu_info, syscall_exit, syscall_get_time_ms, syscall_halt,
    syscall_percpu_stats, syscall_process_list, syscall_reboot, syscall_sleep_ms, syscall_sys_info,
    syscall_user_read, syscall_user_write, syscall_yield,
};
use crate::syscall::font_handlers::syscall_font_set;
use crate::syscall::fs::{
    syscall_dup, syscall_dup2, syscall_dup3, syscall_fcntl, syscall_fs_close, syscall_fs_list,
    syscall_fs_mkdir, syscall_fs_open, syscall_fs_read, syscall_fs_stat, syscall_fs_unlink,
    syscall_fs_write, syscall_fstat, syscall_ioctl, syscall_lseek, syscall_pipe, syscall_pipe2,
    syscall_poll, syscall_rename, syscall_select,
};
use crate::syscall::keymap_handlers::{syscall_keymap_get_name, syscall_keymap_load};
pub use crate::syscall::memory_handlers::{
    syscall_brk, syscall_ftruncate, syscall_memfd_create, syscall_mmap, syscall_mprotect,
    syscall_munmap,
};
use crate::syscall::net_config_handlers::{
    syscall_net_addr_ctl, syscall_net_iface_ctl, syscall_net_monitor, syscall_net_resolver_set,
    syscall_net_route_ctl,
};
use crate::syscall::net_handlers::{
    syscall_accept, syscall_bind, syscall_connect, syscall_getpeername, syscall_getsockname,
    syscall_getsockopt, syscall_listen, syscall_recv, syscall_recvfrom, syscall_recvmsg,
    syscall_resolve, syscall_send, syscall_sendmsg, syscall_sendto, syscall_setsockopt,
    syscall_shutdown, syscall_socket,
};
use crate::syscall::net_query_handlers::syscall_net_query;
pub use crate::syscall::pidfd_handlers::syscall_pidfd_open;
pub use crate::syscall::process_handlers::{
    syscall_arch_prctl, syscall_chdir, syscall_clone, syscall_exec, syscall_fork, syscall_futex,
    syscall_get_cpu_affinity, syscall_get_cpu_count, syscall_get_current_cpu, syscall_getcwd,
    syscall_getegid, syscall_geteuid, syscall_getgid, syscall_getpgid, syscall_getpid,
    syscall_getppid, syscall_getuid, syscall_prlimit64, syscall_set_cpu_affinity, syscall_setpgid,
    syscall_setsid, syscall_sigdefault, syscall_spawn_path, syscall_terminate_task,
    syscall_vhangup, syscall_waitpid,
};
pub use crate::syscall::ring_handlers::{
    syscall_ring_enter, syscall_ring_register, syscall_ring_setup,
};
use crate::syscall::signal::{
    syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask, syscall_rt_sigreturn,
};
pub use crate::syscall::signalfd_handlers::syscall_signalfd;
pub use crate::syscall::test_handlers::{
    syscall_run_userland_tests, syscall_test_panic, syscall_test_report,
};
pub use crate::syscall::ui_handlers::{
    syscall_clipboard_copy, syscall_clipboard_paste, syscall_cursor_move, syscall_cursor_set_image,
    syscall_fb_flip, syscall_fb_info, syscall_getrandom, syscall_input_poll_batch, syscall_openpty,
    syscall_roulette_draw, syscall_roulette_result, syscall_roulette_spin,
    syscall_set_display_mode,
};

/// Build the static syscall dispatch table; unregistered slots stay `None`.
macro_rules! syscall_table {
    (size: $size:expr; $( [$num:expr] => $handler:expr, $name:literal; )*) => {{
        let mut table: [SyscallEntry; $size] = [SyscallEntry {
            handler: None,
            name: ::slopos_ostd::sync::KernelSync::new(core::ptr::null()),
        }; $size];
        $(
            table[$num as usize] = SyscallEntry {
                handler: Some($handler),
                name: ::slopos_ostd::sync::KernelSync::new(
                    concat!($name, "\0").as_ptr() as *const c_char,
                ),
            };
        )*
        table
    }};
}

static SYSCALL_TABLE: [SyscallEntry; SYSCALL_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_TABLE_SIZE;

    [SYSCALL_YIELD]          => syscall_yield,          "yield";
    [SYSCALL_EXIT]           => syscall_exit,           "exit";
    [SYSCALL_WRITE]          => syscall_user_write,     "write";
    [SYSCALL_READ]           => syscall_user_read,      "read";
    [SYSCALL_SLEEP_MS]       => syscall_sleep_ms,       "sleep_ms";
    [SYSCALL_FB_INFO]        => syscall_fb_info,        "fb_info";
    [SYSCALL_GET_TIME_MS]    => syscall_get_time_ms,    "get_time_ms";
    [SYSCALL_SYS_INFO]       => syscall_sys_info,       "sys_info";
    [SYSCALL_NET_QUERY]      => syscall_net_query,      "net_query";
    [SYSCALL_NET_MONITOR]    => syscall_net_monitor,    "net_monitor";
    [SYSCALL_NET_IFACE_CTL]  => syscall_net_iface_ctl,  "net_iface_ctl";
    [SYSCALL_NET_ADDR_CTL]   => syscall_net_addr_ctl,   "net_addr_ctl";
    [SYSCALL_NET_ROUTE_CTL]  => syscall_net_route_ctl,  "net_route_ctl";
    [SYSCALL_NET_RESOLVER_SET] => syscall_net_resolver_set, "net_resolver_set";
    [SYSCALL_HALT]           => syscall_halt,            "halt";
    [SYSCALL_REBOOT]         => syscall_reboot,          "reboot";
    [SYSCALL_CLOCK_GETTIME]  => syscall_clock_gettime,  "clock_gettime";

    [SYSCALL_PROCESS_LIST]  => syscall_process_list,  "process_list";
    [SYSCALL_CPU_INFO]      => syscall_cpu_info,      "cpu_info";
    [SYSCALL_PERCPU_STATS]  => syscall_percpu_stats,  "percpu_stats";

    [SYSCALL_GETRANDOM]       => syscall_getrandom,       "getrandom";
    [SYSCALL_ROULETTE]        => syscall_roulette_spin,   "roulette";
    [SYSCALL_ROULETTE_RESULT] => syscall_roulette_result, "roulette_result";
    [SYSCALL_ROULETTE_DRAW]   => syscall_roulette_draw,   "roulette_draw";

    [SYSCALL_FS_OPEN]   => syscall_fs_open,   "fs_open";
    [SYSCALL_FS_CLOSE]  => syscall_fs_close,  "fs_close";
    [SYSCALL_FS_READ]   => syscall_fs_read,   "fs_read";
    [SYSCALL_FS_WRITE]  => syscall_fs_write,  "fs_write";
    [SYSCALL_FS_STAT]   => syscall_fs_stat,   "fs_stat";
    [SYSCALL_FS_MKDIR]  => syscall_fs_mkdir,  "fs_mkdir";
    [SYSCALL_FS_UNLINK] => syscall_fs_unlink, "fs_unlink";
    [SYSCALL_FS_LIST]   => syscall_fs_list,   "fs_list";
    [SYSCALL_RENAME]    => syscall_rename,    "rename";

    [SYSCALL_SOCKET]  => syscall_socket,  "socket";
    [SYSCALL_BIND]    => syscall_bind,    "bind";
    [SYSCALL_LISTEN]  => syscall_listen,  "listen";
    [SYSCALL_ACCEPT]  => syscall_accept,  "accept";
    [SYSCALL_CONNECT] => syscall_connect, "connect";
    [SYSCALL_SEND]    => syscall_send,    "send";
    [SYSCALL_RECV]    => syscall_recv,    "recv";
    [SYSCALL_SENDTO]  => syscall_sendto,  "sendto";
    [SYSCALL_RECVFROM] => syscall_recvfrom, "recvfrom";
    [SYSCALL_RESOLVE] => syscall_resolve, "resolve";
    [SYSCALL_SETSOCKOPT] => syscall_setsockopt, "setsockopt";
    [SYSCALL_GETSOCKOPT] => syscall_getsockopt, "getsockopt";
    [SYSCALL_SHUTDOWN]   => syscall_shutdown,   "shutdown";
    [SYSCALL_SENDMSG]    => syscall_sendmsg,    "sendmsg";
    [SYSCALL_RECVMSG]    => syscall_recvmsg,    "recvmsg";
    [SYSCALL_GETPEERNAME] => syscall_getpeername, "getpeername";
    [SYSCALL_GETSOCKNAME] => syscall_getsockname, "getsockname";

    [SYSCALL_OPENPTY]       => syscall_openpty,       "openpty";

    [SYSCALL_FB_FLIP]             => syscall_fb_flip,             "fb_flip";
    [SYSCALL_CURSOR_SET_IMAGE]    => syscall_cursor_set_image,    "cursor_set_image";
    [SYSCALL_CURSOR_MOVE]         => syscall_cursor_move,         "cursor_move";
    [SYSCALL_SET_DISPLAY_MODE]    => syscall_set_display_mode,    "set_display_mode";

    [SYSCALL_INPUT_POLL_BATCH]           => syscall_input_poll_batch,           "input_poll_batch";
    [SYSCALL_CLIPBOARD_COPY]             => syscall_clipboard_copy,             "clipboard_copy";
    [SYSCALL_CLIPBOARD_PASTE]            => syscall_clipboard_paste,            "clipboard_paste";

    [SYSCALL_SPAWN_PATH]     => syscall_spawn_path,     "spawn_path";
    [SYSCALL_WAITPID]        => syscall_waitpid,        "waitpid";
    [SYSCALL_TERMINATE_TASK] => syscall_terminate_task,  "terminate_task";
    [SYSCALL_EXEC]           => syscall_exec,            "exec";
    [SYSCALL_FORK]           => syscall_fork,            "fork";
    [SYSCALL_CLONE]          => syscall_clone,           "clone";
    [SYSCALL_FUTEX]          => syscall_futex,           "futex";
    [SYSCALL_ARCH_PRCTL]     => syscall_arch_prctl,      "arch_prctl";

    [SYSCALL_BRK]          => syscall_brk,          "brk";
    [SYSCALL_MMAP]         => syscall_mmap,         "mmap";
    [SYSCALL_MUNMAP]       => syscall_munmap,       "munmap";
    [SYSCALL_MPROTECT]     => syscall_mprotect,     "mprotect";
    [SYSCALL_MEMFD_CREATE] => syscall_memfd_create, "memfd_create";
    [SYSCALL_FTRUNCATE]    => syscall_ftruncate,    "ftruncate";

    [SYSCALL_GET_CPU_COUNT]    => syscall_get_cpu_count,    "get_cpu_count";
    [SYSCALL_GET_CURRENT_CPU]  => syscall_get_current_cpu,  "get_current_cpu";
    [SYSCALL_SET_CPU_AFFINITY] => syscall_set_cpu_affinity, "set_cpu_affinity";
    [SYSCALL_GET_CPU_AFFINITY] => syscall_get_cpu_affinity, "get_cpu_affinity";

    [SYSCALL_GETPID]  => syscall_getpid,  "getpid";
    [SYSCALL_GETPPID] => syscall_getppid, "getppid";
    [SYSCALL_GETUID]  => syscall_getuid,  "getuid";
    [SYSCALL_GETGID]  => syscall_getgid,  "getgid";
    [SYSCALL_GETEUID] => syscall_geteuid, "geteuid";
    [SYSCALL_GETEGID] => syscall_getegid, "getegid";
    [SYSCALL_CHDIR]   => syscall_chdir,   "chdir";
    [SYSCALL_GETCWD]  => syscall_getcwd,  "getcwd";
    [SYSCALL_PRLIMIT64] => syscall_prlimit64, "prlimit64";

    [SYSCALL_RT_SIGACTION]   => syscall_rt_sigaction,   "rt_sigaction";
    [SYSCALL_RT_SIGPROCMASK] => syscall_rt_sigprocmask, "rt_sigprocmask";
    [SYSCALL_KILL]           => syscall_kill,           "kill";
    [SYSCALL_RT_SIGRETURN]   => syscall_rt_sigreturn,   "rt_sigreturn";
    [SYSCALL_SIGDEFAULT]     => syscall_sigdefault,     "sigdefault";

    [SYSCALL_DUP]   => syscall_dup,   "dup";
    [SYSCALL_DUP2]  => syscall_dup2,  "dup2";
    [SYSCALL_DUP3]  => syscall_dup3,  "dup3";
    [SYSCALL_FCNTL] => syscall_fcntl, "fcntl";
    [SYSCALL_LSEEK] => syscall_lseek, "lseek";
    [SYSCALL_FSTAT] => syscall_fstat, "fstat";
    [SYSCALL_POLL]  => syscall_poll,  "poll";
    [SYSCALL_SELECT] => syscall_select, "select";
    [SYSCALL_PIPE] => syscall_pipe, "pipe";
    [SYSCALL_PIPE2] => syscall_pipe2, "pipe2";
    [SYSCALL_IOCTL] => syscall_ioctl, "ioctl";
    [SYSCALL_SETPGID] => syscall_setpgid, "setpgid";
    [SYSCALL_GETPGID] => syscall_getpgid, "getpgid";
    [SYSCALL_SETSID] => syscall_setsid, "setsid";
    [SYSCALL_VHANGUP] => syscall_vhangup, "vhangup";

    [SYSCALL_FONT_SET] => syscall_font_set, "font_set";

    [SYSCALL_KEYMAP_LOAD] => syscall_keymap_load, "keymap_load";
    [SYSCALL_KEYMAP_GET_NAME] => syscall_keymap_get_name, "keymap_get_name";

    [SYSCALL_TEST_REPORT] => syscall_test_report, "test_report";
    [SYSCALL_RUN_USERLAND_TESTS] => syscall_run_userland_tests, "run_userland_tests";
    [SYSCALL_TEST_PANIC] => syscall_test_panic, "test_panic";

    [SYSCALL_RING_SETUP] => syscall_ring_setup, "ring_setup";
    [SYSCALL_RING_ENTER] => syscall_ring_enter, "ring_enter";
    [SYSCALL_RING_REGISTER] => syscall_ring_register, "ring_register";

    [SYSCALL_PIDFD_OPEN] => syscall_pidfd_open, "pidfd_open";

    [SYSCALL_SIGNALFD] => syscall_signalfd, "signalfd";
};

pub fn syscall_lookup(sysno: u64) -> Option<&'static SyscallEntry> {
    let entry = SYSCALL_TABLE.get(sysno as usize)?;
    if entry.handler.is_none() {
        None
    } else {
        Some(entry)
    }
}
