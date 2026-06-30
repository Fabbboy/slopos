//! `keymap` builtin: query, list, and switch the keyboard layout at runtime.
//!
//! `keymap`           — print the active layout name.
//! `keymap list`      — list available `*.layout` files (active marked `*`).
//! `keymap <name>`    — load `/usr/share/keymaps/<name>.layout` and switch to it.
//!
//! Layout files are parsed **in userland** (`slopos_keymap_core::parse`) into the
//! binary `LayoutTable`, serialised, and uploaded to the kernel — the kernel
//! never parses layout text.

use std::fs;

use slopos_keymap_core::{LayoutTable, SERIALIZED_LEN, parse, serialize};

use crate::syscall::keymap::{keymap_get_name, keymap_load};

use super::super::display::{COLOR_ERROR_RED, COLOR_EXEC_GREEN, shell_write, shell_write_idx};

const KEYMAP_DIR: &str = "/usr/share/keymaps";
const PERSIST_PATH: &str = "/etc/keymap";

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

/// The active layout name, queried from the kernel.
fn current_name() -> Option<String> {
    let mut buf = [0u8; 16];
    let n = keymap_get_name(&mut buf);
    if n < 0 {
        return None;
    }
    let n = (n as usize).min(buf.len());
    core::str::from_utf8(&buf[..n]).ok().map(String::from)
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
    let path = format!("{KEYMAP_DIR}/{name}.layout");
    let src = match fs::read(&path) {
        Ok(s) => s,
        Err(_) => {
            err(&format!("keymap: no such layout '{name}'\n"));
            return 1;
        }
    };

    // Parse the text layout into a binary table (in userland), then serialise.
    let mut table = Box::new(LayoutTable::empty());
    if parse(&src, &mut table).is_err() {
        err(&format!("keymap: failed to parse {path}\n"));
        return 1;
    }
    let mut blob = vec![0u8; SERIALIZED_LEN];
    if serialize(&table, &mut blob).is_err() {
        err("keymap: failed to serialise layout\n");
        return 1;
    }

    let rc = keymap_load(&blob);
    if rc < 0 {
        err(&format!("keymap: kernel rejected layout (errno {})\n", -rc));
        return 1;
    }

    // Best-effort persistence (ignored if the root filesystem is read-only); a
    // boot-time applier can re-load this on the next start.
    let _ = fs::write(PERSIST_PATH, name.as_bytes());

    shell_write_idx(
        format!("keymap: switched to {name}\n").as_bytes(),
        COLOR_EXEC_GREEN,
    );
    0
}
