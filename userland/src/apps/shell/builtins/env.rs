use crate::runtime;

use super::super::NL;
use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::env;

fn find_eq(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == b'=')
}

pub fn cmd_export(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        env::for_each(|key, value| {
            shell_write(key);
            shell_write(b"=");
            shell_write(value);
            shell_write(NL.as_bytes());
        });
        return 0;
    }
    for i in 1..argc as usize {
        if i >= argv.len() {
            break;
        }
        let arg = argv[i];
        if arg.is_null() {
            continue;
        }
        let len = runtime::u_strlen(arg);
        if len == 0 {
            continue;
        }
        let bytes = unsafe { core::slice::from_raw_parts(arg, len) };
        if let Some(eq_pos) = find_eq(bytes) {
            let key = &bytes[..eq_pos];
            let value = &bytes[eq_pos + 1..];
            if key.is_empty() {
                shell_write_idx(b"export: invalid identifier\n", COLOR_ERROR_RED);
                return 1;
            }
            env::set(key, value);
        } else {
            if env::get(bytes).is_none() {
                shell_write_idx(b"export: ", COLOR_ERROR_RED);
                shell_write_idx(bytes, COLOR_ERROR_RED);
                shell_write_idx(b": not found\n", COLOR_ERROR_RED);
                return 1;
            }
        }
    }
    0
}

pub fn cmd_unset(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"unset: missing variable name\n", COLOR_ERROR_RED);
        return 1;
    }
    for i in 1..argc as usize {
        if i >= argv.len() {
            break;
        }
        let arg = argv[i];
        if arg.is_null() {
            continue;
        }
        let len = runtime::u_strlen(arg);
        if len == 0 {
            continue;
        }
        let bytes = unsafe { core::slice::from_raw_parts(arg, len) };
        env::unset(bytes);
    }
    0
}

pub fn cmd_env(_argc: i32, _argv: &[*const u8]) -> i32 {
    env::for_each(|key, value| {
        shell_write(key);
        shell_write(b"=");
        shell_write(value);
        shell_write(NL.as_bytes());
    });
    0
}

pub fn cmd_set(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        return cmd_env(argc, argv);
    }
    for i in 1..argc as usize {
        if i >= argv.len() {
            break;
        }
        let arg = argv[i];
        if arg.is_null() {
            continue;
        }
        let len = runtime::u_strlen(arg);
        if len == 0 {
            continue;
        }
        let bytes = unsafe { core::slice::from_raw_parts(arg, len) };
        if let Some(eq_pos) = find_eq(bytes) {
            let key = &bytes[..eq_pos];
            let value = &bytes[eq_pos + 1..];
            if key.is_empty() {
                shell_write_idx(b"set: invalid identifier\n", COLOR_ERROR_RED);
                return 1;
            }
            env::set(key, value);
        } else {
            shell_write_idx(b"set: expected KEY=VALUE\n", COLOR_ERROR_RED);
            return 1;
        }
    }
    0
}
