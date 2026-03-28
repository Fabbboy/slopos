use core::ffi::c_char;
use core::ptr;

use slopos_abi::syscall::*;

use crate::syscall::common::SyscallEntry;
pub use crate::syscall::core_handlers::{
    syscall_clock_gettime, syscall_cpu_info, syscall_exit, syscall_get_time_ms, syscall_halt,
    syscall_net_info, syscall_net_scan, syscall_percpu_stats, syscall_process_list, syscall_reboot,
    syscall_sleep_ms, syscall_sys_info, syscall_user_read, syscall_user_write, syscall_yield,
};
use crate::syscall::font_handlers::syscall_font_set;
use crate::syscall::fs::{
    syscall_dup, syscall_dup2, syscall_dup3, syscall_fcntl, syscall_fs_close, syscall_fs_list,
    syscall_fs_mkdir, syscall_fs_open, syscall_fs_read, syscall_fs_stat, syscall_fs_unlink,
    syscall_fs_write, syscall_fstat, syscall_ioctl, syscall_lseek, syscall_pipe, syscall_pipe2,
    syscall_poll, syscall_rename, syscall_select,
};
pub use crate::syscall::memory_handlers::{
    syscall_brk, syscall_mmap, syscall_mprotect, syscall_munmap,
};
use crate::syscall::net_handlers::{
    syscall_accept, syscall_bind, syscall_connect, syscall_getsockopt, syscall_listen,
    syscall_recv, syscall_recvfrom, syscall_resolve, syscall_send, syscall_sendto,
    syscall_setsockopt, syscall_shutdown, syscall_socket,
};
pub use crate::syscall::process_handlers::{
    syscall_arch_prctl, syscall_chdir, syscall_clone, syscall_exec, syscall_fork, syscall_futex,
    syscall_get_cpu_affinity, syscall_get_cpu_count, syscall_get_current_cpu, syscall_getcwd,
    syscall_getegid, syscall_geteuid, syscall_getgid, syscall_getpgid, syscall_getpid,
    syscall_getppid, syscall_getuid, syscall_set_cpu_affinity, syscall_setpgid, syscall_setsid,
    syscall_spawn_path, syscall_terminate_task, syscall_vhangup, syscall_waitpid,
};
use crate::syscall::signal::{
    syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask, syscall_rt_sigreturn,
};
pub use crate::syscall::ui_handlers::{
    syscall_buffer_age, syscall_clipboard_copy, syscall_clipboard_paste, syscall_drain_queue,
    syscall_enumerate_windows, syscall_fb_flip, syscall_fb_info, syscall_input_get_button_state,
    syscall_input_get_pointer_pos, syscall_input_has_events, syscall_input_poll,
    syscall_input_poll_batch, syscall_input_request_close, syscall_input_set_focus,
    syscall_input_set_focus_with_offset, syscall_mark_frames_done, syscall_open_tty_fd,
    syscall_openpty, syscall_poll_frame_done, syscall_raise_window, syscall_random_next,
    syscall_roulette_draw, syscall_roulette_result, syscall_roulette_spin,
    syscall_set_cursor_shape, syscall_set_window_position, syscall_set_window_state,
    syscall_shm_acquire, syscall_shm_create, syscall_shm_create_with_format, syscall_shm_destroy,
    syscall_shm_get_formats, syscall_shm_map, syscall_shm_poll_released, syscall_shm_release,
    syscall_shm_unmap, syscall_surface_attach, syscall_surface_commit, syscall_surface_damage,
    syscall_surface_frame, syscall_surface_set_app_id, syscall_surface_set_parent,
    syscall_surface_set_rel_pos, syscall_surface_set_role, syscall_surface_set_title,
    syscall_tty_read, syscall_tty_write,
};

/// Build the static syscall dispatch table from a compact registration list.
///
/// Each entry maps a syscall number constant to its handler function and a
/// debug name string. Unregistered slots remain `{ handler: None, name: null }`.
macro_rules! syscall_table {
    (size: $size:expr; $( [$num:expr] => $handler:expr, $name:literal; )*) => {{
        let mut table: [SyscallEntry; $size] = [SyscallEntry {
            handler: None,
            name: core::ptr::null(),
        }; $size];
        $(
            table[$num as usize] = SyscallEntry {
                handler: Some($handler),
                name: concat!($name, "\0").as_ptr() as *const c_char,
            };
        )*
        table
    }};
}

static SYSCALL_TABLE: [SyscallEntry; SYSCALL_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_TABLE_SIZE;

    // Core
    [SYSCALL_YIELD]          => syscall_yield,          "yield";
    [SYSCALL_EXIT]           => syscall_exit,           "exit";
    [SYSCALL_WRITE]          => syscall_user_write,     "write";
    [SYSCALL_READ]           => syscall_user_read,      "read";
    [SYSCALL_SLEEP_MS]       => syscall_sleep_ms,       "sleep_ms";
    [SYSCALL_FB_INFO]        => syscall_fb_info,        "fb_info";
    [SYSCALL_GET_TIME_MS]    => syscall_get_time_ms,    "get_time_ms";
    [SYSCALL_SYS_INFO]       => syscall_sys_info,       "sys_info";
    [SYSCALL_NET_SCAN]       => syscall_net_scan,       "net_scan";
    [SYSCALL_NET_INFO]       => syscall_net_info,       "net_info";
    [SYSCALL_HALT]           => syscall_halt,            "halt";
    [SYSCALL_REBOOT]         => syscall_reboot,          "reboot";
    [SYSCALL_CLOCK_GETTIME]  => syscall_clock_gettime,  "clock_gettime";

    [SYSCALL_PROCESS_LIST]  => syscall_process_list,  "process_list";
    [SYSCALL_CPU_INFO]      => syscall_cpu_info,      "cpu_info";
    [SYSCALL_PERCPU_STATS]  => syscall_percpu_stats,  "percpu_stats";

    // Random / Roulette
    [SYSCALL_RANDOM_NEXT]     => syscall_random_next,     "random_next";
    [SYSCALL_ROULETTE]        => syscall_roulette_spin,   "roulette";
    [SYSCALL_ROULETTE_RESULT] => syscall_roulette_result, "roulette_result";
    [SYSCALL_ROULETTE_DRAW]   => syscall_roulette_draw,   "roulette_draw";

    // Filesystem
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

    // TTY
    [SYSCALL_OPENPTY]       => syscall_openpty,       "openpty";
    [SYSCALL_TTY_READ]      => syscall_tty_read,      "tty_read";
    [SYSCALL_TTY_WRITE]     => syscall_tty_write,     "tty_write";
    [SYSCALL_OPEN_TTY_FD]   => syscall_open_tty_fd,   "open_tty_fd";

    // Window management
    [SYSCALL_ENUMERATE_WINDOWS]   => syscall_enumerate_windows,   "enumerate_windows";
    [SYSCALL_SET_WINDOW_POSITION] => syscall_set_window_position, "set_window_position";
    [SYSCALL_SET_WINDOW_STATE]    => syscall_set_window_state,    "set_window_state";
    [SYSCALL_SET_CURSOR_SHAPE]    => syscall_set_cursor_shape,    "set_cursor_shape";
    [SYSCALL_RAISE_WINDOW]        => syscall_raise_window,        "raise_window";

    // Surface / Compositor
    [SYSCALL_SURFACE_COMMIT]      => syscall_surface_commit,      "surface_commit";
    [SYSCALL_SURFACE_ATTACH]      => syscall_surface_attach,      "surface_attach";
    [SYSCALL_SURFACE_FRAME]       => syscall_surface_frame,       "surface_frame";
    [SYSCALL_POLL_FRAME_DONE]     => syscall_poll_frame_done,     "poll_frame_done";
    [SYSCALL_MARK_FRAMES_DONE]    => syscall_mark_frames_done,    "mark_frames_done";
    [SYSCALL_SURFACE_DAMAGE]      => syscall_surface_damage,      "surface_damage";
    [SYSCALL_BUFFER_AGE]          => syscall_buffer_age,          "buffer_age";
    [SYSCALL_SURFACE_SET_ROLE]    => syscall_surface_set_role,    "surface_set_role";
    [SYSCALL_SURFACE_SET_PARENT]  => syscall_surface_set_parent,  "surface_set_parent";
    [SYSCALL_SURFACE_SET_REL_POS] => syscall_surface_set_rel_pos, "surface_set_rel_pos";
    [SYSCALL_SURFACE_SET_TITLE]    => syscall_surface_set_title,    "surface_set_title";
    [SYSCALL_SURFACE_SET_APP_ID]  => syscall_surface_set_app_id,  "surface_set_app_id";
    [SYSCALL_FB_FLIP]             => syscall_fb_flip,             "fb_flip";
    [SYSCALL_DRAIN_QUEUE]         => syscall_drain_queue,         "drain_queue";

    // Shared memory
    [SYSCALL_SHM_CREATE]             => syscall_shm_create,             "shm_create";
    [SYSCALL_SHM_MAP]                => syscall_shm_map,                "shm_map";
    [SYSCALL_SHM_UNMAP]              => syscall_shm_unmap,              "shm_unmap";
    [SYSCALL_SHM_DESTROY]            => syscall_shm_destroy,            "shm_destroy";
    [SYSCALL_SHM_ACQUIRE]            => syscall_shm_acquire,            "shm_acquire";
    [SYSCALL_SHM_RELEASE]            => syscall_shm_release,            "shm_release";
    [SYSCALL_SHM_POLL_RELEASED]      => syscall_shm_poll_released,      "shm_poll_released";
    [SYSCALL_SHM_GET_FORMATS]        => syscall_shm_get_formats,        "shm_get_formats";
    [SYSCALL_SHM_CREATE_WITH_FORMAT] => syscall_shm_create_with_format, "shm_create_with_format";

    // Input
    [SYSCALL_INPUT_POLL]                 => syscall_input_poll,                 "input_poll";
    [SYSCALL_INPUT_POLL_BATCH]           => syscall_input_poll_batch,           "input_poll_batch";
    [SYSCALL_INPUT_HAS_EVENTS]           => syscall_input_has_events,           "input_has_events";
    [SYSCALL_INPUT_SET_FOCUS]            => syscall_input_set_focus,            "input_set_focus";
    [SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET] => syscall_input_set_focus_with_offset, "input_set_focus_with_offset";
    [SYSCALL_INPUT_GET_POINTER_POS]      => syscall_input_get_pointer_pos,      "input_get_pointer_pos";
    [SYSCALL_INPUT_GET_BUTTON_STATE]     => syscall_input_get_button_state,     "input_get_button_state";
    [SYSCALL_INPUT_REQUEST_CLOSE]        => syscall_input_request_close,        "input_request_close";
    [SYSCALL_CLIPBOARD_COPY]             => syscall_clipboard_copy,             "clipboard_copy";
    [SYSCALL_CLIPBOARD_PASTE]            => syscall_clipboard_paste,            "clipboard_paste";

    // Task management
    [SYSCALL_SPAWN_PATH]     => syscall_spawn_path,     "spawn_path";
    [SYSCALL_WAITPID]        => syscall_waitpid,        "waitpid";
    [SYSCALL_TERMINATE_TASK] => syscall_terminate_task,  "terminate_task";
    [SYSCALL_EXEC]           => syscall_exec,            "exec";
    [SYSCALL_FORK]           => syscall_fork,            "fork";
    [SYSCALL_CLONE]          => syscall_clone,           "clone";
    [SYSCALL_FUTEX]          => syscall_futex,           "futex";
    [SYSCALL_ARCH_PRCTL]     => syscall_arch_prctl,      "arch_prctl";

    // Memory
    [SYSCALL_BRK]      => syscall_brk,      "brk";
    [SYSCALL_MMAP]     => syscall_mmap,     "mmap";
    [SYSCALL_MUNMAP]   => syscall_munmap,   "munmap";
    [SYSCALL_MPROTECT] => syscall_mprotect, "mprotect";

    // SMP / CPU affinity
    [SYSCALL_GET_CPU_COUNT]    => syscall_get_cpu_count,    "get_cpu_count";
    [SYSCALL_GET_CURRENT_CPU]  => syscall_get_current_cpu,  "get_current_cpu";
    [SYSCALL_SET_CPU_AFFINITY] => syscall_set_cpu_affinity, "set_cpu_affinity";
    [SYSCALL_GET_CPU_AFFINITY] => syscall_get_cpu_affinity, "get_cpu_affinity";

    // Process identity
    [SYSCALL_GETPID]  => syscall_getpid,  "getpid";
    [SYSCALL_GETPPID] => syscall_getppid, "getppid";
    [SYSCALL_GETUID]  => syscall_getuid,  "getuid";
    [SYSCALL_GETGID]  => syscall_getgid,  "getgid";
    [SYSCALL_GETEUID] => syscall_geteuid, "geteuid";
    [SYSCALL_GETEGID] => syscall_getegid, "getegid";
    [SYSCALL_CHDIR]   => syscall_chdir,   "chdir";
    [SYSCALL_GETCWD]  => syscall_getcwd,  "getcwd";

    [SYSCALL_RT_SIGACTION]   => syscall_rt_sigaction,   "rt_sigaction";
    [SYSCALL_RT_SIGPROCMASK] => syscall_rt_sigprocmask, "rt_sigprocmask";
    [SYSCALL_KILL]           => syscall_kill,           "kill";
    [SYSCALL_RT_SIGRETURN]   => syscall_rt_sigreturn,   "rt_sigreturn";

    // File descriptor operations
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

    // Font management
    [SYSCALL_FONT_SET] => syscall_font_set, "font_set";
};

pub fn syscall_lookup(sysno: u64) -> *const SyscallEntry {
    if (sysno as usize) >= SYSCALL_TABLE.len() {
        return ptr::null();
    }
    let entry = &SYSCALL_TABLE[sysno as usize];
    if entry.handler.is_none() {
        ptr::null()
    } else {
        entry as *const SyscallEntry
    }
}
