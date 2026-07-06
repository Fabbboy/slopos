//! `keymap` builtin: query, list, and switch the keyboard layout at runtime.
//!
//! `keymap`           — print the active layout name.
//! `keymap list`      — list available `*.layout` files (active marked `*`).
//! `keymap <name>`    — load `/usr/share/keymaps/<name>.layout` and switch to it.
//!
//! The file → parse → serialise → upload pipeline lives in [`crate::keymap`]
//! (shared with the boot-time applier in init); the kernel never parses
//! layout text.

use std::fs;

use crate::keymap::{KEYMAP_DIR, PERSIST_PATH, current_name, load_layout_by_name};

use super::super::display::{COLOR_ERROR_RED, COLOR_EXEC_GREEN, shell_write, shell_write_idx};

pub fn cmd_keymap(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        return print_current();
    }
    match argv.get(1).copied().unwrap_or(b"") {
        b"list" | b"-l" | b"--list" => list_layouts(),
        name => match core::str::from_utf8(name) {
            Ok(name) if !name.is_empty() => set_layout(name),
            _ => {
                err("keymap: invalid layout name\n");
                1
            }
        },
    }
}

fn err(msg: &str) {
    shell_write_idx(msg.as_bytes(), COLOR_ERROR_RED);
}

fn print_current() -> i32 {
    match current_name() {
        Some(name) => {
            shell_write(name.as_bytes());
            shell_write(b"\n");
            0
        }
        None => {
            err("keymap: cannot query active layout\n");
            1
        }
    }
}

fn list_layouts() -> i32 {
    let active = current_name();
    let entries = match fs::read_dir(KEYMAP_DIR) {
        Ok(e) => e,
        Err(_) => {
            err(&format!("keymap: cannot read {KEYMAP_DIR}\n"));
            return 1;
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name();
        if let Some(stem) = fname.to_string_lossy().strip_suffix(".layout") {
            names.push(String::from(stem));
        }
    }
    names.sort();
    for name in &names {
        if active.as_deref() == Some(name.as_str()) {
            shell_write_idx(format!("* {name}\n").as_bytes(), COLOR_EXEC_GREEN);
        } else {
            shell_write(format!("  {name}\n").as_bytes());
        }
    }
    0
}

fn set_layout(name: &str) -> i32 {
    if let Err(cause) = load_layout_by_name(name) {
        err(&format!("keymap: {cause}: '{name}'\n"));
        return 1;
    }

    // Best-effort persistence (ignored if the root filesystem is read-only);
    // init re-applies it on the next boot.
    let _ = fs::write(PERSIST_PATH, name.as_bytes());

    shell_write_idx(
        format!("keymap: switched to {name}\n").as_bytes(),
        COLOR_EXEC_GREEN,
    );
    0
}
