//! File system builtin commands: ls, cat, write, mkdir, rm, cd, pwd,
//! stat, touch, cp, mv, head, tail, wc, hexdump, tee, diff.

use core::cmp;
use core::option::Option;
use core::option::Option::{None, Some};
use core::result::Result::{Err, Ok};

use std::env;
use std::fs::{self as stdfs, File, OpenOptions};
use std::io::{Read, Write};

use crate::syscall::fs as sys_fs;
use crate::syscall::{POLLIN, UserPollFd};

use super::super::buffers;
use super::super::display::{COLOR_DIR_BLUE, COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::interrupt;
use super::super::jobs;
use super::super::parser::normalize_path;
use super::super::{
    ERR_MISSING_FILE, ERR_MISSING_OPERAND, ERR_MISSING_TEXT, ERR_NO_SUCH, ERR_TOO_MANY_ARGS, NL,
    PATH_TOO_LONG, SHELL_IO_MAX,
};

struct LsEntry {
    name: std::vec::Vec<u8>,
    is_directory: bool,
    size: u64,
}

pub fn cmd_ls(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let has_path = argc == 2 && !argv[1].is_empty();

    buffers::with_path_buf(|path_buf| {
        if !has_path {
            let cwd = super::super::cwd_bytes();
            let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
            let len = cwd_len.min(path_buf.len() - 1);
            path_buf[..len].copy_from_slice(&cwd[..len]);
            path_buf[len] = 0;
        } else if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let path_str = path_buf_to_str(path_buf);
        let metadata = match stdfs::metadata(path_str) {
            Ok(metadata) => metadata,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };
        if !metadata.is_dir() {
            shell_write_idx(b"ls: not a directory\n", COLOR_ERROR_RED);
            return 1;
        }

        let read_dir = match stdfs::read_dir(path_str) {
            Ok(read_dir) => read_dir,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };

        let mut entries: std::vec::Vec<LsEntry> = std::vec::Vec::new();
        for dir_entry in read_dir {
            let dir_entry = match dir_entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            let name = match dir_entry.file_name().into_string() {
                Ok(name) => name.into_bytes(),
                Err(_) => continue,
            };

            if name == b"." || name == b".." {
                continue;
            }

            let entry_meta = match dir_entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            entries.push(LsEntry {
                name,
                is_directory: entry_meta.is_dir(),
                size: entry_meta.len(),
            });
        }

        let count = entries.len();
        if count > 1 {
            for i in 0..count - 1 {
                for j in 0..count - 1 - i {
                    if entry_name_gt(&entries[j], &entries[j + 1]) {
                        entries.swap(j, j + 1);
                    }
                }
            }
        }

        if entries.is_empty() {
            shell_write(b"(empty)\n");
            return 0;
        }

        for entry in &entries {
            if entry.is_directory {
                shell_write_idx(&entry.name, COLOR_DIR_BLUE);
                shell_write_idx(b"/\n", COLOR_DIR_BLUE);
            } else {
                shell_write(&entry.name);
                shell_write(b" (");
                jobs::write_u64(entry.size);
                shell_write(b")\n");
            }
        }

        0
    })
}

pub fn cmd_cat(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc == 1 {
        let mut buf = [0u8; SHELL_IO_MAX];
        loop {
            let n = match sys_fs::read_slice(0, &mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            if !shell_write(&buf[..n]) {
                break;
            }
        }
        return 0;
    }

    let mut rc = 0;
    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_empty() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }

            let path_str = path_buf_to_str(path_buf);
            let mut file = match File::open(path_str) {
                Ok(file) => file,
                Err(_) => {
                    shell_write_idx(b"cat: ", COLOR_ERROR_RED);
                    shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                    return 1;
                }
            };

            let mut tmp = [0u8; SHELL_IO_MAX + 1];
            let r = match file.read(&mut tmp[..SHELL_IO_MAX]) {
                Ok(n) => n,
                Err(_) => {
                    shell_write_idx(b"cat: read error\n", COLOR_ERROR_RED);
                    return 1;
                }
            };
            let len = cmp::min(r, tmp.len() - 1);
            tmp[len] = 0;
            if len == 0 {
                return 0;
            }
            shell_write(&tmp[..len]);
            if tmp[len - 1] != b'\n' {
                shell_write(NL.as_bytes());
            }
            if r == SHELL_IO_MAX {
                shell_write(b"[truncated]\n");
            }
            0
        });
        if result != 0 {
            rc = result;
        }
    }
    rc
}

pub fn cmd_write(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_FILE.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc < 3 {
        shell_write_idx(ERR_MISSING_TEXT.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let text = argv[2];
        if text.is_empty() {
            shell_write_idx(ERR_MISSING_TEXT.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let len = text.len().min(SHELL_IO_MAX);
        let path_str = path_buf_to_str(path_buf);
        if stdfs::write(path_str, &text[..len]).is_err() {
            shell_write_idx(b"write failed\n", COLOR_ERROR_RED);
            return 1;
        }

        0
    })
}

pub fn cmd_mkdir(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_OPERAND.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
        if stdfs::create_dir(path_buf_to_str(path_buf)).is_err() {
            shell_write_idx(b"mkdir failed\n", COLOR_ERROR_RED);
            return 1;
        }
        0
    })
}

pub fn cmd_rm(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_OPERAND.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
        if stdfs::remove_file(path_buf_to_str(path_buf)).is_err() {
            shell_write_idx(b"rm failed\n", COLOR_ERROR_RED);
            return 1;
        }
        0
    })
}

pub fn cmd_cd(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut resolved = [0u8; 256];

    if argc < 2 {
        resolved[0] = b'/';
        resolved[1] = 0;
    } else {
        let arg = argv[1];
        if arg.is_empty() {
            resolved[0] = b'/';
            resolved[1] = 0;
        } else if arg == b".." {
            let cwd = super::super::cwd_bytes();
            let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
            if cwd_len <= 1 {
                resolved[0] = b'/';
                resolved[1] = 0;
            } else {
                let mut last_slash = 0;
                for i in 0..cwd_len {
                    if cwd[i] == b'/' && i > 0 {
                        last_slash = i;
                    }
                }
                if last_slash == 0 {
                    resolved[0] = b'/';
                    resolved[1] = 0;
                } else {
                    resolved[..last_slash].copy_from_slice(&cwd[..last_slash]);
                    resolved[last_slash] = 0;
                }
            }
        } else if normalize_path(arg, &mut resolved) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    }

    let resolved_len = resolved.iter().position(|&b| b == 0).unwrap_or(0);
    if resolved_len == 0 {
        resolved[0] = b'/';
        resolved[1] = 0;
    }

    let path_str = path_buf_to_str(&resolved);
    let metadata = match stdfs::metadata(path_str) {
        Ok(metadata) => metadata,
        Err(_) => {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    };
    if !metadata.is_dir() {
        shell_write_idx(b"cd: not a directory\n", COLOR_ERROR_RED);
        return 1;
    }
    if env::set_current_dir(path_str).is_err() {
        shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    super::super::cwd_set(&resolved);
    0
}

pub fn cmd_pwd(_argc: i32, _argv: &[&[u8]]) -> i32 {
    if let Ok(path) = env::current_dir() {
        if let Some(path_str) = path.to_str() {
            shell_write(path_str.as_bytes());
            shell_write(NL.as_bytes());
            return 0;
        }
    }

    let cwd = super::super::cwd_bytes();
    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(1);
    shell_write(&cwd[..cwd_len]);
    shell_write(NL.as_bytes());
    0
}

pub fn cmd_stat(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_OPERAND.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 2 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let metadata = match stdfs::metadata(path_buf_to_str(path_buf)) {
            Ok(metadata) => metadata,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };

        let path_len = path_buf.iter().position(|&b| b == 0).unwrap_or(0);
        shell_write(b"  File: ");
        shell_write(&path_buf[..path_len]);
        shell_write(NL.as_bytes());

        shell_write(b"  Type: ");
        if metadata.is_file() {
            shell_write(b"regular file");
        } else if metadata.is_dir() {
            shell_write(b"directory");
        } else {
            shell_write(b"unknown");
        }
        shell_write(NL.as_bytes());

        shell_write(b"  Size: ");
        jobs::write_u64(metadata.len());
        shell_write(NL.as_bytes());

        0
    })
}

pub fn cmd_touch(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_OPERAND.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut rc = 0;
    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_empty() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }

            if OpenOptions::new()
                .create(true)
                .write(true)
                .open(path_buf_to_str(path_buf))
                .is_err()
            {
                shell_write_idx(b"touch: cannot create file\n", COLOR_ERROR_RED);
                return 1;
            }
            0
        });
        if result != 0 {
            rc = result;
        }
    }
    rc
}

pub fn cmd_cp(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 3 {
        shell_write_idx(b"cp: missing operand\n", COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut src_path = [0u8; 256];
    let mut dst_path = [0u8; 256];

    if normalize_path(argv[1], &mut src_path) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if normalize_path(argv[2], &mut dst_path) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    if paths_equal(&src_path, &dst_path) {
        shell_write_idx(
            b"cp: source and destination are the same\n",
            COLOR_ERROR_RED,
        );
        return 1;
    }

    let src_str = path_buf_to_str(&src_path);
    let dst_str = path_buf_to_str(&dst_path);

    let metadata = match stdfs::metadata(src_str) {
        Ok(metadata) => metadata,
        Err(_) => {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    };
    if metadata.is_dir() {
        shell_write_idx(b"cannot copy directory\n", COLOR_ERROR_RED);
        return 1;
    }

    match stdfs::copy(src_str, dst_str) {
        Ok(_) => 0,
        Err(e) => {
            shell_write_idx(
                format!("cp: {src_str} -> {dst_str}: {e}\n").as_bytes(),
                COLOR_ERROR_RED,
            );
            1
        }
    }
}

pub fn cmd_mv(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 3 {
        shell_write_idx(b"mv: missing operand\n", COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut src_path = [0u8; 256];
    let mut dst_path = [0u8; 256];

    if normalize_path(argv[1], &mut src_path) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if normalize_path(argv[2], &mut dst_path) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    if paths_equal(&src_path, &dst_path) {
        shell_write_idx(
            b"mv: source and destination are the same\n",
            COLOR_ERROR_RED,
        );
        return 1;
    }

    let src_str = path_buf_to_str(&src_path);
    let dst_str = path_buf_to_str(&dst_path);

    if stdfs::rename(src_str, dst_str).is_ok() {
        return 0;
    }

    let metadata = match stdfs::metadata(src_str) {
        Ok(metadata) => metadata,
        Err(_) => {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    };
    if metadata.is_dir() {
        shell_write_idx(b"cannot copy directory\n", COLOR_ERROR_RED);
        return 1;
    }

    if let Err(e) = stdfs::copy(src_str, dst_str) {
        shell_write_idx(
            format!("mv: copy failed: {e}\n").as_bytes(),
            COLOR_ERROR_RED,
        );
        return 1;
    }

    if stdfs::remove_file(src_str).is_err() {
        shell_write_idx(b"mv: cannot remove source\n", COLOR_ERROR_RED);
        return 1;
    }
    0
}

pub fn cmd_head(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let (use_stdin, n_lines, file_arg_idx) = if argc < 2 {
        (true, 10usize, 0usize)
    } else if argc == 2 {
        match jobs::parse_u32_arg(argv[1]) {
            Some(n) if n > 0 => (true, n as usize, 0),
            _ => (false, 10usize, 1),
        }
    } else {
        let n = match jobs::parse_u32_arg(argv[2]) {
            Some(n) if n > 0 => n as usize,
            _ => {
                shell_write_idx(b"head: invalid line count\n", COLOR_ERROR_RED);
                return 1;
            }
        };
        (false, n, 1)
    };

    if use_stdin {
        return head_from_fd(0, n_lines);
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[file_arg_idx], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let mut file = match File::open(path_buf_to_str(path_buf)) {
            Ok(file) => file,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };

        head_from_reader(&mut file, n_lines)
    })
}

fn head_from_fd(fd: i32, n_lines: usize) -> i32 {
    let mut lines_seen = 0usize;
    let mut buf = [0u8; SHELL_IO_MAX];
    let mut done = false;

    while !done {
        let n = match sys_fs::read_slice(fd, &mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }

        let mut output_end = n;
        for (i, &b) in buf[..n].iter().enumerate() {
            if b == b'\n' {
                lines_seen += 1;
                if lines_seen >= n_lines {
                    output_end = i + 1;
                    done = true;
                    break;
                }
            }
        }
        shell_write(&buf[..output_end]);
    }
    0
}

fn head_from_reader<R: Read>(reader: &mut R, n_lines: usize) -> i32 {
    let mut lines_seen = 0usize;
    let mut buf = [0u8; SHELL_IO_MAX];
    let mut done = false;

    while !done {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }

        let mut output_end = n;
        for (i, &b) in buf[..n].iter().enumerate() {
            if b == b'\n' {
                lines_seen += 1;
                if lines_seen >= n_lines {
                    output_end = i + 1;
                    done = true;
                    break;
                }
            }
        }
        shell_write(&buf[..output_end]);
    }
    0
}

pub fn cmd_tail(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let (use_stdin, n_lines, file_arg_idx) = if argc < 2 {
        (true, 10usize, 0usize)
    } else if argc == 2 {
        match jobs::parse_u32_arg(argv[1]) {
            Some(n) if n > 0 => (true, n as usize, 0),
            _ => (false, 10usize, 1),
        }
    } else {
        let n = match jobs::parse_u32_arg(argv[2]) {
            Some(n) if n > 0 => n as usize,
            _ => {
                shell_write_idx(b"tail: invalid line count\n", COLOR_ERROR_RED);
                return 1;
            }
        };
        (false, n, 1)
    };

    if use_stdin {
        return tail_from_fd(0, n_lines);
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[file_arg_idx], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let mut file = match File::open(path_buf_to_str(path_buf)) {
            Ok(file) => file,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };

        tail_from_reader(&mut file, n_lines)
    })
}

fn tail_from_fd(fd: i32, n_lines: usize) -> i32 {
    const TAIL_BUF: usize = 4096;
    let mut data = [0u8; TAIL_BUF];
    let mut total = 0usize;

    loop {
        if total >= TAIL_BUF {
            break;
        }
        let chunk = (TAIL_BUF - total).min(SHELL_IO_MAX);
        let n = match sys_fs::read_slice(fd, &mut data[total..total + chunk]) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        total += n;
    }

    write_tail_output(&data[..total], n_lines)
}

fn tail_from_reader<R: Read>(reader: &mut R, n_lines: usize) -> i32 {
    const TAIL_BUF: usize = 4096;
    let mut data = [0u8; TAIL_BUF];
    let mut total = 0usize;

    loop {
        if total >= TAIL_BUF {
            break;
        }
        let chunk = (TAIL_BUF - total).min(SHELL_IO_MAX);
        let n = match reader.read(&mut data[total..total + chunk]) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        total += n;
    }

    write_tail_output(&data[..total], n_lines)
}

fn write_tail_output(data: &[u8], n_lines: usize) -> i32 {
    if data.is_empty() {
        return 0;
    }

    let mut count = 0usize;
    let scan_start = if data[data.len() - 1] == b'\n' {
        data.len().saturating_sub(1)
    } else {
        data.len()
    };

    let mut start = 0usize;
    let mut pos = scan_start;
    while pos > 0 {
        pos -= 1;
        if data[pos] == b'\n' {
            count += 1;
            if count >= n_lines {
                start = pos + 1;
                break;
            }
        }
    }

    shell_write(&data[start..]);
    if data[data.len() - 1] != b'\n' {
        shell_write(NL.as_bytes());
    }
    0
}

pub fn cmd_wc(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        let mut lines = 0usize;
        let mut words = 0usize;
        let mut chars = 0usize;
        let mut in_word = false;
        let mut buf = [0u8; SHELL_IO_MAX];
        loop {
            let n = match sys_fs::read_slice(0, &mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            chars += n;
            for &b in &buf[..n] {
                if b == b'\n' {
                    lines += 1;
                }
                if is_wc_space(b) {
                    if in_word {
                        words += 1;
                        in_word = false;
                    }
                } else {
                    in_word = true;
                }
            }
        }
        if in_word {
            words += 1;
        }
        write_wc_line(lines, words, chars, b"");
        return 0;
    }

    let mut total_lines = 0usize;
    let mut total_words = 0usize;
    let mut total_chars = 0usize;
    let file_count = (argc - 1) as usize;
    let mut rc = 0;

    for i in 1..argc as usize {
        if i >= argv.len() || argv[i].is_empty() {
            continue;
        }
        let result = buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[i], path_buf) != 0 {
                shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }

            let mut file = match File::open(path_buf_to_str(path_buf)) {
                Ok(file) => file,
                Err(_) => {
                    shell_write_idx(b"wc: ", COLOR_ERROR_RED);
                    shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                    return 1;
                }
            };

            let mut lines = 0usize;
            let mut words = 0usize;
            let mut chars = 0usize;
            let mut in_word = false;
            let mut buf = [0u8; SHELL_IO_MAX];

            loop {
                let n = match file.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                chars += n;
                for &b in &buf[..n] {
                    if b == b'\n' {
                        lines += 1;
                    }
                    if is_wc_space(b) {
                        if in_word {
                            words += 1;
                            in_word = false;
                        }
                    } else {
                        in_word = true;
                    }
                }
            }
            if in_word {
                words += 1;
            }

            let path_len = path_buf.iter().position(|&b| b == 0).unwrap_or(0);
            write_wc_line(lines, words, chars, &path_buf[..path_len]);

            total_lines += lines;
            total_words += words;
            total_chars += chars;
            0
        });
        if result != 0 {
            rc = result;
        }
    }

    if file_count > 1 {
        write_wc_line(total_lines, total_words, total_chars, b"total");
    }
    rc
}

pub fn cmd_hexdump(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(ERR_MISSING_FILE.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let max_bytes: usize = if argc >= 3 {
        match jobs::parse_u32_arg(argv[2]) {
            Some(n) => n as usize,
            None => {
                shell_write_idx(b"hexdump: invalid byte count\n", COLOR_ERROR_RED);
                return 1;
            }
        }
    } else {
        256
    };

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }

        let mut file = match File::open(path_buf_to_str(path_buf)) {
            Ok(file) => file,
            Err(_) => {
                shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
                return 1;
            }
        };

        let read_len = max_bytes.min(SHELL_IO_MAX);
        let mut buf = [0u8; SHELL_IO_MAX];
        let n = match file.read(&mut buf[..read_len]) {
            Ok(n) => n,
            Err(_) => {
                shell_write_idx(b"hexdump: read error\n", COLOR_ERROR_RED);
                return 1;
            }
        };

        if n == 0 {
            shell_write(b"(empty)\n");
            return 0;
        }

        let mut offset = 0usize;
        while offset < n {
            let line_len = (n - offset).min(16);

            write_hex_u16(offset as u16);
            shell_write(b": ");

            for i in 0..16usize {
                if i < line_len {
                    write_hex_byte(buf[offset + i]);
                    shell_write(b" ");
                } else {
                    shell_write(b"   ");
                }
                if i == 7 {
                    shell_write(b" ");
                }
            }

            shell_write(b" |");
            for i in 0..line_len {
                let b = buf[offset + i];
                if (0x20..=0x7E).contains(&b) {
                    let ch = [b];
                    shell_write(&ch);
                } else {
                    shell_write(b".");
                }
            }
            shell_write(b"|\n");

            offset += 16;
        }

        0
    })
}

pub fn cmd_diff(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 3 {
        shell_write_idx(b"diff: missing operand\n", COLOR_ERROR_RED);
        return 1;
    }
    if argc > 3 {
        shell_write_idx(ERR_TOO_MANY_ARGS.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    let mut path1 = [0u8; 256];
    let mut path2 = [0u8; 256];

    if normalize_path(argv[1], &mut path1) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }
    if normalize_path(argv[2], &mut path2) != 0 {
        shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
        return 1;
    }

    const DIFF_BUF: usize = 2048;

    let data1 = match stdfs::read(path_buf_to_str(&path1)) {
        Ok(data) => data,
        Err(_) => {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    };
    let data2 = match stdfs::read(path_buf_to_str(&path2)) {
        Ok(data) => data,
        Err(_) => {
            shell_write_idx(ERR_NO_SUCH.as_bytes(), COLOR_ERROR_RED);
            return 1;
        }
    };

    let len1 = data1.len().min(DIFF_BUF);
    let len2 = data2.len().min(DIFF_BUF);
    let data1 = &data1[..len1];
    let data2 = &data2[..len2];

    let mut pos1 = 0usize;
    let mut pos2 = 0usize;
    let mut line_num = 1usize;
    let mut differ = false;

    while pos1 < len1 || pos2 < len2 {
        let (end1, line1_len) = if pos1 < len1 {
            find_line_end(&data1[pos1..len1])
        } else {
            (0, 0)
        };
        let (end2, line2_len) = if pos2 < len2 {
            find_line_end(&data2[pos2..len2])
        } else {
            (0, 0)
        };

        let line1 = &data1[pos1..pos1 + line1_len];
        let line2 = &data2[pos2..pos2 + line2_len];

        if line1 != line2 {
            differ = true;
            jobs::write_u64(line_num as u64);
            shell_write(b"c");
            jobs::write_u64(line_num as u64);
            shell_write(NL.as_bytes());
            shell_write(b"< ");
            shell_write(line1);
            shell_write(NL.as_bytes());
            shell_write(b"---\n");
            shell_write(b"> ");
            shell_write(line2);
            shell_write(NL.as_bytes());
        }

        if end1 == 0 && end2 == 0 {
            break;
        }
        pos1 += end1;
        pos2 += end2;
        line_num += 1;
    }

    if differ { 1 } else { 0 }
}

pub fn cmd_tee(argc: i32, argv: &[&[u8]]) -> i32 {
    let mut append = false;
    let mut file_arg: Option<usize> = None;

    let mut i = 1usize;
    while i < argc as usize {
        if i >= argv.len() || argv[i].is_empty() {
            i += 1;
            continue;
        }
        let arg = argv[i];
        if arg == b"-a" {
            append = true;
        } else {
            file_arg = Some(i);
        }
        i += 1;
    }

    let mut file = if let Some(idx) = file_arg {
        buffers::with_path_buf(|path_buf| {
            if normalize_path(argv[idx], path_buf) != 0 {
                shell_write_idx(PATH_TOO_LONG.as_bytes(), COLOR_ERROR_RED);
                return None;
            }

            let mut options = OpenOptions::new();
            options.create(true).write(true);
            if append {
                options.append(true);
            } else {
                options.truncate(true);
            }

            match options.open(path_buf_to_str(path_buf)) {
                Ok(file) => Some(file),
                Err(_) => {
                    shell_write_idx(b"tee: cannot open file\n", COLOR_ERROR_RED);
                    None
                }
            }
        })
    } else {
        None
    };

    if file_arg.is_some() && file.is_none() {
        return 1;
    }

    let mut buf = [0u8; SHELL_IO_MAX];
    loop {
        // Wait for stdin readiness with a short timeout so the loop keeps
        // pumping terminal input — that is what lets the line discipline
        // see Ctrl+C and interrupt a tee that is blocked on its stdin.
        let mut pfds = [UserPollFd {
            fd: 0,
            events: POLLIN,
            revents: 0,
        }];
        let ready = sys_fs::poll(&mut pfds, 10).unwrap_or(0);
        if interrupt::take_pending() {
            return interrupt::EXIT_INTERRUPTED;
        }
        if ready == 0 {
            continue;
        }
        let n = match sys_fs::read_slice(0, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        shell_write(&buf[..n]);
        if let Some(file) = file.as_mut() {
            if file.write_all(&buf[..n]).is_err() {
                shell_write_idx(b"tee: write error\n", COLOR_ERROR_RED);
                return 1;
            }
        }
    }

    0
}

fn path_buf_to_str(path: &[u8]) -> &str {
    let path_len = path.iter().position(|&b| b == 0).unwrap_or(0);
    core::str::from_utf8(&path[..path_len]).unwrap_or("/")
}

fn entry_name_gt(a: &LsEntry, b: &LsEntry) -> bool {
    let min_len = a.name.len().min(b.name.len());

    for i in 0..min_len {
        let ca = a.name[i].to_ascii_lowercase();
        let cb = b.name[i].to_ascii_lowercase();
        if ca != cb {
            return ca > cb;
        }
    }
    a.name.len() > b.name.len()
}

fn find_line_end(data: &[u8]) -> (usize, usize) {
    if data.is_empty() {
        return (0, 0);
    }
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            return (i + 1, i);
        }
    }
    (data.len(), data.len())
}

fn is_wc_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

fn write_wc_line(lines: usize, words: usize, chars: usize, name: &[u8]) {
    shell_write(b"  ");
    jobs::write_u64(lines as u64);
    shell_write(b"  ");
    jobs::write_u64(words as u64);
    shell_write(b"  ");
    jobs::write_u64(chars as u64);
    if !name.is_empty() {
        shell_write(b" ");
        shell_write(name);
    }
    shell_write(NL.as_bytes());
}

fn write_hex_byte(b: u8) {
    shell_write(format!("{b:02x}").as_bytes());
}

fn write_hex_u16(val: u16) {
    shell_write(format!("{val:04x}").as_bytes());
}

fn paths_equal(a: &[u8], b: &[u8]) -> bool {
    let a_len = a.iter().position(|&c| c == 0).unwrap_or(a.len());
    let b_len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    if a_len != b_len {
        return false;
    }
    a[..a_len] == b[..b_len]
}
