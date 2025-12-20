#![allow(static_mut_refs)]

use core::cmp;
use core::ffi::{c_char, c_void};
use core::ptr;

use crate::runtime;
use crate::syscall::{
    USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFbInfo, UserFsEntry,
    UserFsList, UserRect, UserSysInfo, UserText, sys_fb_info, sys_font_draw, sys_fs_close,
    sys_fs_list, sys_fs_mkdir, sys_fs_open, sys_fs_read, sys_fs_unlink, sys_fs_write,
    sys_gfx_fill_rect, sys_halt, sys_read_char, sys_sys_info, sys_write,
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

const FONT_CHAR_WIDTH: i32 = 8;
const FONT_CHAR_HEIGHT: i32 = 16;
const SHELL_BG_COLOR: u32 = 0x0000_0000;
const SHELL_FG_COLOR: u32 = 0xE6E6_E6FF;
const SHELL_TAB_WIDTH: i32 = 4;

#[unsafe(link_section = ".user_bss")]
static mut LINE_BUF: [u8; 256] = [0; 256];
#[unsafe(link_section = ".user_bss")]
static mut TOKEN_STORAGE: [[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS] =
    [[0; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS];
#[unsafe(link_section = ".user_bss")]
static mut PATH_BUF: [u8; SHELL_PATH_BUF] = [0; SHELL_PATH_BUF];
#[unsafe(link_section = ".user_bss")]
static mut LIST_ENTRIES: [UserFsEntry; 32] = [UserFsEntry::new(); 32];

struct ShellConsole {
    enabled: bool,
    width: i32,
    height: i32,
    cols: i32,
    rows: i32,
    cursor_col: i32,
    cursor_row: i32,
    fg: u32,
    bg: u32,
}

impl ShellConsole {
    const fn disabled() -> Self {
        Self {
            enabled: false,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            cursor_col: 0,
            cursor_row: 0,
            fg: SHELL_FG_COLOR,
            bg: SHELL_BG_COLOR,
        }
    }

    fn init(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        self.cols = width / FONT_CHAR_WIDTH;
        self.rows = height / FONT_CHAR_HEIGHT;
        if self.cols <= 0 || self.rows <= 0 {
            self.enabled = false;
            return;
        }
        self.enabled = true;
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.fg = SHELL_FG_COLOR;
        self.bg = SHELL_BG_COLOR;
        self.clear();
    }

    fn clear(&mut self) {
        if !self.enabled {
            return;
        }
        let rect = UserRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
            color: self.bg,
        };
        let _ = sys_gfx_fill_rect(&rect);
        self.cursor_col = 0;
        self.cursor_row = 0;
    }

    fn clear_line(&mut self, row: i32) {
        if !self.enabled {
            return;
        }
        let y = row * FONT_CHAR_HEIGHT;
        let rect = UserRect {
            x: 0,
            y,
            width: self.width,
            height: FONT_CHAR_HEIGHT,
            color: self.bg,
        };
        let _ = sys_gfx_fill_rect(&rect);
    }

    fn set_cursor(&mut self, col: i32, row: i32) {
        self.cursor_col = col.clamp(0, self.cols.saturating_sub(1));
        self.cursor_row = row.clamp(0, self.rows.saturating_sub(1));
    }

    fn cursor(&self) -> (i32, i32) {
        (self.cursor_col, self.cursor_row)
    }

    fn new_line(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;
        if self.cursor_row >= self.rows {
            self.clear();
        }
    }

    fn backspace(&mut self) {
        if !self.enabled {
            return;
        }
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cols.saturating_sub(1);
        } else {
            return;
        }
        let x = self.cursor_col * FONT_CHAR_WIDTH;
        let y = self.cursor_row * FONT_CHAR_HEIGHT;
        let rect = UserRect {
            x,
            y,
            width: FONT_CHAR_WIDTH,
            height: FONT_CHAR_HEIGHT,
            color: self.bg,
        };
        let _ = sys_gfx_fill_rect(&rect);
    }

    fn draw_char(&mut self, c: u8) {
        if !self.enabled {
            return;
        }
        let x = self.cursor_col * FONT_CHAR_WIDTH;
        let y = self.cursor_row * FONT_CHAR_HEIGHT;
        let buf = [c];
        let text = UserText {
            x,
            y,
            fg_color: self.fg,
            bg_color: self.bg,
            str_ptr: buf.as_ptr() as *const c_char,
            len: 1,
        };
        let _ = sys_font_draw(&text);
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.new_line();
        }
    }

    fn write(&mut self, buf: &[u8]) {
        if !self.enabled {
            return;
        }
        for &b in buf {
            match b {
                b'\n' => self.new_line(),
                b'\r' => self.cursor_col = 0,
                b'\t' => {
                    for _ in 0..SHELL_TAB_WIDTH {
                        self.draw_char(b' ');
                    }
                }
                b'\x08' => self.backspace(),
                0x20..=0x7E => self.draw_char(b),
                _ => {}
            }
        }
    }
}

#[unsafe(link_section = ".user_bss")]
static mut SHELL_CONSOLE: ShellConsole = ShellConsole::disabled();

#[unsafe(link_section = ".user_text")]
fn shell_console_init() {
    let mut info = UserFbInfo::default();
    if sys_fb_info(&mut info) != 0 || info.width == 0 || info.height == 0 {
        return;
    }
    unsafe {
        SHELL_CONSOLE.init(info.width as i32, info.height as i32);
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_clear() {
    unsafe {
        if SHELL_CONSOLE.enabled {
            SHELL_CONSOLE.clear();
        }
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_write(buf: &[u8]) {
    unsafe {
        if SHELL_CONSOLE.enabled {
            SHELL_CONSOLE.write(buf);
        }
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_write(buf: &[u8]) {
    let _ = sys_write(buf);
    shell_console_write(buf);
}

#[unsafe(link_section = ".user_text")]
fn shell_echo_char(c: u8) {
    let buf = [c];
    let _ = sys_write(&buf);
    shell_console_write(&buf);
}

#[unsafe(link_section = ".user_text")]
fn shell_console_get_cursor() -> (i32, i32) {
    unsafe {
        if SHELL_CONSOLE.enabled {
            SHELL_CONSOLE.cursor()
        } else {
            (0, 0)
        }
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_set_cursor(col: i32, row: i32) {
    unsafe {
        if SHELL_CONSOLE.enabled {
            SHELL_CONSOLE.set_cursor(col, row);
        }
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_clear_line(row: i32) {
    unsafe {
        if SHELL_CONSOLE.enabled {
            SHELL_CONSOLE.clear_line(row);
        }
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_redraw_input(line_row: i32, buf: &[u8]) {
    shell_console_set_cursor(0, line_row);
    shell_console_clear_line(line_row);
    shell_console_write(PROMPT);
    shell_console_write(buf);
    shell_console_set_cursor(PROMPT.len() as i32 + buf.len() as i32, line_row);
}

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
fn u_streq_slice(a: *const u8, b: &[u8]) -> bool {
    if a.is_null() {
        return b.is_empty();
    }
    let len = runtime::u_strlen(a);
    if len != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < len {
        unsafe {
            if *a.add(i) != b[i] {
                return false;
            }
        }
        i += 1;
    }
    true
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
        if u_streq_slice(name, entry.name) {
            return Some(entry);
        }
    }
    None
}

#[unsafe(link_section = ".user_text")]
fn print_kv(key: &[u8], value: u64) {
    if !key.is_empty() {
        let _ = shell_write(key);
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
    let _ = shell_write(&tmp[..idx]);
    let _ = shell_write(NL);
}

#[unsafe(link_section = ".user_text")]
fn cmd_help(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = shell_write(HELP_HEADER);
    for entry in BUILTINS {
        let _ = shell_write(b"  ");
        let _ = shell_write(entry.name);
        let _ = shell_write(b" - ");
        if !entry.desc.is_empty() {
            let _ = shell_write(entry.desc);
        }
        let _ = shell_write(NL);
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
            let _ = shell_write(b" ");
        }
        let len = runtime::u_strlen(arg);
        let _ = shell_write(unsafe { core::slice::from_raw_parts(arg, len) });
        first = false;
    }
    let _ = shell_write(NL);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_clear(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = shell_write(b"\x1B[2J\x1B[H");
    shell_console_clear();
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_halt(_argc: i32, _argv: &[*const u8]) -> i32 {
    let _ = shell_write(HALTED);
    sys_halt();
}

#[unsafe(link_section = ".user_text")]
fn cmd_info(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_sys_info(&mut info) != 0 {
        let _ = shell_write(b"info: failed\n");
        return 1;
    }
    let _ = shell_write(b"Kernel information:\n");
    let _ = shell_write(b"  Memory: total pages=");
    print_kv(b"", info.total_pages as u64);
    let _ = shell_write(b"  Free pages=");
    print_kv(b"", info.free_pages as u64);
    let _ = shell_write(b"  Allocated pages=");
    print_kv(b"", info.allocated_pages as u64);
    let _ = shell_write(b"  Tasks: total=");
    print_kv(b"", info.total_tasks as u64);
    let _ = shell_write(b"  Active tasks=");
    print_kv(b"", info.active_tasks as u64);
    let _ = shell_write(b"  Task ctx switches=");
    print_kv(b"", info.task_context_switches);
    let _ = shell_write(b"  Scheduler: switches=");
    print_kv(b"", info.scheduler_context_switches);
    let _ = shell_write(b"  Yields=");
    print_kv(b"", info.scheduler_yields);
    let _ = shell_write(b"  Ready=");
    print_kv(b"", info.ready_tasks as u64);
    let _ = shell_write(b"  schedule() calls=");
    print_kv(b"", info.schedule_calls as u64);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_ls(argc: i32, argv: &[*const u8]) -> i32 {
    if argc > 2 {
        let _ = shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let path_ptr = if argc == 2 { argv[1] } else { ptr::null() };
    let path_buf_guard = unsafe { &mut PATH_BUF };

    let path = if path_ptr.is_null() {
        b"/\0".as_ptr()
    } else {
        if normalize_path(path_ptr, path_buf_guard) != 0 {
            let _ = shell_write(PATH_TOO_LONG);
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
        let _ = shell_write(ERR_NO_SUCH);
        return 1;
    }

    for i in 0..list.count {
        let entry = unsafe { &LIST_ENTRIES[i as usize] };
        if entry.r#type == 1 {
            let _ = shell_write(b"[");
            let _ =
                shell_write(&entry.name[..runtime::u_strnlen(entry.name.as_ptr(), entry.name.len())]);
            let _ = shell_write(b"]\n");
        } else {
            let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
            let _ = shell_write(&entry.name[..name_len]);
            let _ = shell_write(b" (");
            print_kv(b"", entry.size as u64);
        }
    }

    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_cat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 2 {
        let _ = shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = shell_write(PATH_TOO_LONG);
        return 1;
    }
    let mut tmp = [0u8; SHELL_IO_MAX + 1];
    let fd = sys_fs_open(path_buf_guard.as_ptr() as *const c_char, USER_FS_OPEN_READ);
    if fd < 0 {
        let _ = shell_write(ERR_NO_SUCH);
        return 1;
    }
    let r = sys_fs_read(fd as i32, tmp.as_mut_ptr() as *mut c_void, SHELL_IO_MAX);
    let _ = sys_fs_close(fd as i32);
    if r < 0 {
        let _ = shell_write(ERR_NO_SUCH);
        return 1;
    }
    let len = cmp::min(r as usize, tmp.len() - 1);
    tmp[len] = 0;
    let _ = shell_write(&tmp[..len]);
    if r as usize == SHELL_IO_MAX {
        let _ = shell_write(b"\n[truncated]\n");
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_write(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc < 3 {
        let _ = shell_write(ERR_MISSING_TEXT);
        return 1;
    }
    if argc > 3 {
        let _ = shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = shell_write(PATH_TOO_LONG);
        return 1;
    }
    let text = argv[2];
    if text.is_null() {
        let _ = shell_write(ERR_MISSING_TEXT);
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
        let _ = shell_write(b"write failed\n");
        return 1;
    }
    let w = sys_fs_write(fd as i32, text as *const c_void, len);
    let _ = sys_fs_close(fd as i32);
    if w < 0 || w as usize != len {
        let _ = shell_write(b"write failed\n");
        return 1;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_mkdir(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        let _ = shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = shell_write(PATH_TOO_LONG);
        return 1;
    }
    if sys_fs_mkdir(path_buf_guard.as_ptr() as *const c_char) != 0 {
        let _ = shell_write(b"mkdir failed\n");
        return 1;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_rm(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        let _ = shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        let _ = shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }
    let path_buf_guard = unsafe { &mut PATH_BUF };
    if normalize_path(argv[1], path_buf_guard) != 0 {
        let _ = shell_write(PATH_TOO_LONG);
        return 1;
    }
    if sys_fs_unlink(path_buf_guard.as_ptr() as *const c_char) != 0 {
        let _ = shell_write(b"rm failed\n");
        return 1;
    }
    0
}
#[unsafe(link_section = ".user_text")]
pub fn shell_user_main(_arg: *mut c_void) {
    shell_console_init();
    shell_console_clear();
    let _ = shell_write(WELCOME);
    loop {
        let (_, line_row) = shell_console_get_cursor();
        let _ = shell_write(PROMPT);
        unsafe {
            runtime::u_memset(LINE_BUF.as_mut_ptr() as *mut c_void, 0, LINE_BUF.len());
        }
        let mut len = 0usize;
        loop {
            let rc = sys_read_char();
            if rc < 0 {
                continue;
            }
            let c = rc as u8;
            if c == b'\n' || c == b'\r' {
                shell_echo_char(b'\n');
                break;
            }
            if c == b'\x08' || c == 0x7f {
                if len > 0 {
                    len -= 1;
                    shell_redraw_input(line_row, unsafe { &LINE_BUF[..len] });
                }
                continue;
            }
            if c < 0x20 {
                continue;
            }
            if len + 1 >= unsafe { LINE_BUF.len() } {
                continue;
            }
            unsafe {
                LINE_BUF[len] = c;
            }
            len += 1;
            shell_redraw_input(line_row, unsafe { &LINE_BUF[..len] });
        }
        let capped = cmp::min(len, unsafe { LINE_BUF.len() - 1 });
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
            let _ = shell_write(UNKNOWN_CMD);
        }
    }
}
