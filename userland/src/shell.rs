#![allow(static_mut_refs)]

use core::cmp;
use core::ffi::{c_char, c_void};
use core::ptr;

use crate::runtime;
use crate::syscall::{
    USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFsEntry, UserFsList,
    UserSysInfo, sys_fs_close, sys_fs_list, sys_fs_mkdir, sys_fs_open, sys_fs_read, sys_fs_unlink,
    sys_fs_write, sys_halt, sys_read, sys_sys_info, sys_write,
};

const SHELL_MAX_TOKENS: usize = 16;
const SHELL_MAX_TOKEN_LENGTH: usize = 64;
const SHELL_PATH_BUF: usize = 128;
const SHELL_IO_MAX: usize = 512;

#[unsafe(link_section = ".user_rodata")]
static PROMPT: &[u8] = b"$ ";
#[unsafe(link_section = ".user_rodata")]
static NL: &[u8] = b"\n";
#[unsafe(link_section = ".user_rodata")]
static WELCOME: &[u8] = b"SlopOS Shell v0.1 (userland)\n";
#[unsafe(link_section = ".user_rodata")]
static HELP_HEADER: &[u8] = b"Available commands:\n";
#[unsafe(link_section = ".user_rodata")]
static UNKNOWN_CMD: &[u8] = b"Unknown command. Type 'help'.\n";
#[unsafe(link_section = ".user_rodata")]
static PATH_TOO_LONG: &[u8] = b"path too long\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_NO_SUCH: &[u8] = b"No such file or directory\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_TOO_MANY_ARGS: &[u8] = b"too many arguments\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_OPERAND: &[u8] = b"missing operand\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_FILE: &[u8] = b"missing file operand\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_TEXT: &[u8] = b"missing text operand\n";
#[unsafe(link_section = ".user_rodata")]
static HALTED: &[u8] = b"Shell requested shutdown...\n";

#[unsafe(link_section = ".user_bss")]
static mut LINE_BUF: [u8; 256] = [0; 256];
#[unsafe(link_section = ".user_bss")]
static mut TOKEN_STORAGE: [[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS] =
    [[0; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS];
#[unsafe(link_section = ".user_bss")]
static mut PATH_BUF: [u8; SHELL_PATH_BUF] = [0; SHELL_PATH_BUF];
#[unsafe(link_section = ".user_bss")]
static mut LIST_ENTRIES: [UserFsEntry; 32] = [UserFsEntry::new(); 32];

type BuiltinFn = fn(argc: i32, argv: &[*const u8]) -> i32;

struct BuiltinEntry {
    name: &'static [u8],
    desc: &'static [u8],
    func: BuiltinFn,
}

#[unsafe(link_section = ".user_rodata")]
static BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: b"help",
        func: cmd_help,
        desc: b"List available commands",
    },
    BuiltinEntry {
        name: b"echo",
        func: cmd_echo,
        desc: b"Print arguments back to the terminal",
    },
    BuiltinEntry {
        name: b"clear",
        func: cmd_clear,
        desc: b"Clear the terminal display",
    },
    BuiltinEntry {
        name: b"halt",
        func: cmd_halt,
        desc: b"Shut down the kernel",
    },
    BuiltinEntry {
        name: b"info",
        func: cmd_info,
        desc: b"Show kernel memory and scheduler stats",
    },
    BuiltinEntry {
        name: b"ls",
        func: cmd_ls,
        desc: b"List directory contents",
    },
    BuiltinEntry {
        name: b"cat",
        func: cmd_cat,
        desc: b"Display file contents",
    },
    BuiltinEntry {
        name: b"write",
        func: cmd_write,
        desc: b"Write text to a file",
    },
    BuiltinEntry {
        name: b"mkdir",
        func: cmd_mkdir,
        desc: b"Create a directory",
    },
    BuiltinEntry {
        name: b"rm",
        func: cmd_rm,
        desc: b"Remove a file",
    },
];

#[inline(always)]
fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

#[unsafe(link_section = ".user_text")]
fn u_strcmp(a: *const u8, b: *const u8) -> i32 {
    if a.is_null() && b.is_null() {
        return 0;
    }
    if a.is_null() {
        return -1;
    }
    if b.is_null() {
        return 1;
    }
    let mut pa = a;
    let mut pb = b;
    loop {
        let ca = unsafe { *pa };
        let cb = unsafe { *pb };
        if ca != cb || ca == 0 || cb == 0 {
            return ca as i32 - cb as i32;
        }
        pa = unsafe { pa.add(1) };
        pb = unsafe { pb.add(1) };
    }
}

#[unsafe(link_section = ".user_text")]
fn normalize_path(input: *const u8, buffer: &mut [u8]) -> i32 {
    if buffer.is_empty() {
        return -1;
    }
    if input.is_null() || unsafe { *input } == 0 {
        buffer[0] = b'/';
        if buffer.len() > 1 {
            buffer[1] = 0;
        }
        return 0;
    }

    unsafe {
        if *input == b'/' {
            let len = runtime::u_strnlen(input, buffer.len().saturating_sub(1));
            if len >= buffer.len() {
                return -1;
            }
            ptr::copy_nonoverlapping(input, buffer.as_mut_ptr(), len);
            buffer[len] = 0;
            return 0;
        }
    }

    let maxlen = buffer.len().saturating_sub(2);
    let len = runtime::u_strnlen(input, maxlen);
    if len > maxlen {
        return -1;
    }
    buffer[0] = b'/';
    unsafe {
        ptr::copy_nonoverlapping(input, buffer.as_mut_ptr().add(1), len);
    }
    let term_idx = cmp::min(len + 1, buffer.len() - 1);
    buffer[term_idx] = 0;
    0
}

#[unsafe(link_section = ".user_text")]
fn shell_parse_line(line: &[u8], tokens: &mut [*const u8]) -> i32 {
    if line.is_empty() || tokens.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut cursor = 0usize;
    while cursor < line.len() {
        while cursor < line.len() && is_space(line[cursor]) {
            cursor += 1;
        }
        if cursor >= line.len() || line[cursor] == 0 {
            break;
        }
        let start = cursor;
        while cursor < line.len() && line[cursor] != 0 && !is_space(line[cursor]) {
            cursor += 1;
        }
        if count >= tokens.len() {
            continue;
        }
        let token_len = cmp::min(cursor - start, SHELL_MAX_TOKEN_LENGTH - 1);
        unsafe {
            let dst = &mut TOKEN_STORAGE[count][..token_len];
            dst.copy_from_slice(&line[start..start + token_len]);
            TOKEN_STORAGE[count][token_len] = 0;
            tokens[count] = TOKEN_STORAGE[count].as_ptr();
        }
        count += 1;
    }
    if count < tokens.len() {
        tokens[count] = ptr::null();
    }
    count as i32
}

#[unsafe(link_section = ".user_text")]
fn find_builtin(name: *const u8) -> Option<&'static BuiltinEntry> {
    for entry in BUILTINS {
        if u_strcmp(entry.name.as_ptr(), name) == 0 {
            return Some(entry);
        }
    }
    None
}

#[unsafe(link_section = ".user_text")]
fn print_kv(key: &[u8], value: u64) {
    if !key.is_empty() {
        let _ = sys_write(key);
    }
    let mut tmp = [0u8; 32];
    let mut idx = 0usize;
    if value == 0 {
        tmp[idx] = b'0';
        idx += 1;
    } else {
        let mut n = value;
        let mut rev = [0u8; 32];
        let mut r = 0usize;
        while n != 0 && r < rev.len() {
            rev[r] = b'0' + (n % 10) as u8;
            n /= 10;
            r += 1;
        }
        while r > 0 && idx < tmp.len() {
            idx += 1;
            tmp[idx - 1] = rev[r - 1];
            r -= 1;
        }
    }
    let _ = sys_write(&tmp[..idx]);
    let _ = sys_write(NL);
}

#[unsafe(link_section = ".user_text")]
fn cmd_help(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = sys_write(HELP_HEADER);
    for entry in BUILTINS {
        let _ = sys_write(b"  ");
        let _ = sys_write(entry.name);
        let _ = sys_write(b" - ");
        if !entry.desc.is_empty() {
            let _ = sys_write(entry.desc);
        }
        let _ = sys_write(NL);
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_echo(argc: i32, argv: &[*const u8]) -> i32 {
    let mut first = true;
    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() {
            break;
        }
        let arg = argv[idx];
        if arg.is_null() {
            continue;
        }
        if !first {
            let _ = sys_write(b" ");
        }
        let len = runtime::u_strlen(arg);
        let _ = sys_write(unsafe { core::slice::from_raw_parts(arg, len) });
        first = false;
    }
    let _ = sys_write(NL);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_clear(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = sys_write(b"\x1B[2J\x1B[H");
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_halt(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = sys_write(HALTED);
    sys_halt();
}

#[unsafe(link_section = ".user_text")]
fn cmd_info(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_sys_info(&mut info) != 0 {
        let _ = sys_write(b"info: failed\n");
        return 1;
    }
    let _ = sys_write(b"Kernel information:\n");
    let _ = sys_write(b"  Memory: total pages=");
    print_kv(b"", info.total_pages as u64);
    let _ = sys_write(b"  Free pages=");
    print_kv(b"", info.free_pages as u64);
    let _ = sys_write(b"  Allocated pages=");
    print_kv(b"", info.allocated_pages as u64);
    let _ = sys_write(b"  Tasks: total=");
    print_kv(b"", info.total_tasks as u64);
    let _ = sys_write(b"  Active tasks=");
    print_kv(b"", info.active_tasks as u64);
    let _ = sys_write(b"  Task ctx switches=");
    print_kv(b"", info.task_context_switches);
    let _ = sys_write(b"  Scheduler: switches=");
    print_kv(b"", info.scheduler_context_switches);
    let _ = sys_write(b"  Yields=");
    print_kv(b"", info.scheduler_yields);
    let _ = sys_write(b"  Ready=");
    print_kv(b"", info.ready_tasks as u64);
    let _ = sys_write(b"  schedule() calls=");
    print_kv(b"", info.schedule_calls as u64);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_ls(argc: i32, argv: &[*const u8]) -> i32 {
    if argc > 2 {
        let _ = sys_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let path_ptr = if argc == 2 { argv[1] } else { ptr::null() };
    let path_buf_guard = unsafe { &mut PATH_BUF };

    let path = if path_ptr.is_null() {
        b"/\0".as_ptr()
    } else {
        if normalize_path(path_ptr, path_buf_guard) != 0 {
            let _ = sys_write(PATH_TOO_LONG);
            return 1;
        }
        path_buf_guard.as_ptr()
    };

    let mut list = UserFsList {
        entries: unsafe { LIST_ENTRIES.as_mut_ptr() },
        max_entries: (unsafe { LIST_ENTRIES.len() }) as u32,
        count: 0,
    };

    if sys_fs_list(path as *const c_char, &mut list) != 0 {
        let _ = sys_write(ERR_NO_SUCH);
        return 1;
    }

    for i in 0..list.count {
        let entry = unsafe { &LIST_ENTRIES[i as usize] };
        if entry.r#type == 1 {
            let _ = sys_write(b"[");
            let _ =
                sys_write(&entry.name[..runtime::u_strnlen(entry.name.as_ptr(), entry.name.len())]);
            let _ = sys_write(b"]\n");
        } else {
            let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
            let _ = sys_write(&entry.name[..name_len]);
            let _ = sys_write(b" (");
            print_kv(b"", entry.size as u64);
        }
    }

    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_cat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = sys_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 2 {
        let _ = sys_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = sys_write(PATH_TOO_LONG);
        return 1;
    }
    let mut tmp = [0u8; SHELL_IO_MAX + 1];
    let fd = sys_fs_open(path_buf_guard.as_ptr() as *const c_char, USER_FS_OPEN_READ);
    if fd < 0 {
        let _ = sys_write(ERR_NO_SUCH);
        return 1;
    }
    let r = sys_fs_read(fd as i32, tmp.as_mut_ptr() as *mut c_void, SHELL_IO_MAX);
    let _ = sys_fs_close(fd as i32);
    if r < 0 {
        let _ = sys_write(ERR_NO_SUCH);
        return 1;
    }
    let len = cmp::min(r as usize, tmp.len() - 1);
    tmp[len] = 0;
    let _ = sys_write(&tmp[..len]);
    if r as usize == SHELL_IO_MAX {
        let _ = sys_write(b"\n[truncated]\n");
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_write(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = sys_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc < 3 {
        let _ = sys_write(ERR_MISSING_TEXT);
        return 1;
    }
    if argc > 3 {
        let _ = sys_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = sys_write(PATH_TOO_LONG);
        return 1;
    }
    let text = argv[2];
    if text.is_null() {
        let _ = sys_write(ERR_MISSING_TEXT);
        return 1;
    }
    let mut len = runtime::u_strlen(text);
    if len > SHELL_IO_MAX {
        len = SHELL_IO_MAX;
    }
    let fd = sys_fs_open(
        path_buf_guard.as_ptr() as *const c_char,
        USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT,
    );
    if fd < 0 {
        let _ = sys_write(b"write failed\n");
        return 1;
    }
    let w = sys_fs_write(fd as i32, text as *const c_void, len);
    let _ = sys_fs_close(fd as i32);
    if w < 0 || w as usize != len {
        let _ = sys_write(b"write failed\n");
        return 1;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_mkdir(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = sys_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        let _ = sys_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = sys_write(PATH_TOO_LONG);
        return 1;
    }
    if sys_fs_mkdir(path_buf_guard.as_ptr() as *const c_char) != 0 {
        let _ = sys_write(b"mkdir failed\n");
        return 1;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_rm(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = sys_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        let _ = sys_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = sys_write(PATH_TOO_LONG);
        return 1;
    }
    if sys_fs_unlink(path_buf_guard.as_ptr() as *const c_char) != 0 {
        let _ = sys_write(b"rm failed\n");
        return 1;
    }
    0
}
#[unsafe(link_section = ".user_text")]
pub fn shell_user_main(_arg: *mut c_void) {
    let _ = sys_write(WELCOME);
    loop {
        let _ = sys_write(PROMPT);
        unsafe {
            runtime::u_memset(LINE_BUF.as_mut_ptr() as *mut c_void, 0, LINE_BUF.len());
        }
        let len = sys_read(unsafe { &mut LINE_BUF[..LINE_BUF.len() - 1] });
        if len <= 0 {
            continue;
        }
        let capped = cmp::min(len as usize, unsafe { LINE_BUF.len() - 1 });
        unsafe { LINE_BUF[capped] = 0 };

        let mut tokens: [*const u8; SHELL_MAX_TOKENS] = [ptr::null(); SHELL_MAX_TOKENS];
        let token_count = shell_parse_line(unsafe { &LINE_BUF }, &mut tokens);
        if token_count <= 0 {
            continue;
        }
        let builtin = find_builtin(tokens[0]);
        if let Some(b) = builtin {
            (b.func)(token_count, &tokens);
        } else {
            let _ = sys_write(UNKNOWN_CMD);
        }
    }
}
