//! `keymap` — query, list, and switch the keyboard layout.
//!
//! ```text
//! keymap           print the active layout name
//! keymap list      list available layouts (active marked `*`)
//! keymap <name>    load /usr/share/keymaps/<name>.layout and switch to it
//! ```
//!
//! A program rather than a shell builtin because installing a layout needs
//! `TASK_FLAG_CONSOLE_ADMIN`, which the kernel confers on this path and on no
//! caller.
//!
//! The file → parse → serialise → upload pipeline lives in [`crate::keymap`],
//! shared with the boot-time applier in init; the kernel never parses layout
//! text.

use std::fs;

use crate::keymap::{KEYMAP_DIR, PERSIST_PATH, current_name, load_layout_by_name};

pub fn keymap_user_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => print_current(),
        Some("list" | "-l" | "--list") => list_layouts(),
        Some(name) if !name.is_empty() => set_layout(name),
        Some(_) => {
            eprintln!("keymap: invalid layout name");
            1
        }
    }
}

fn print_current() -> i32 {
    match current_name() {
        Some(name) => {
            println!("{name}");
            0
        }
        None => {
            eprintln!("keymap: cannot query active layout");
            1
        }
    }
}

fn list_layouts() -> i32 {
    let active = current_name();
    let entries = match fs::read_dir(KEYMAP_DIR) {
        Ok(e) => e,
        Err(_) => {
            eprintln!("keymap: cannot read {KEYMAP_DIR}");
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
            println!("* {name}");
        } else {
            println!("  {name}");
        }
    }
    0
}

fn set_layout(name: &str) -> i32 {
    if let Err(cause) = load_layout_by_name(name) {
        eprintln!("keymap: {cause}: '{name}'");
        return 1;
    }

    // Best-effort: fails on a read-only root; init re-applies it on next boot.
    let _ = fs::write(PERSIST_PATH, name.as_bytes());

    println!("keymap: switched to {name}");
    0
}
