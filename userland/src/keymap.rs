//! Keyboard-layout helpers: the file → parse → serialise → upload pipeline.
//!
//! Layout files are parsed in userland into a binary `LayoutTable` and uploaded
//! via `SYSCALL_KEYMAP_LOAD`; the kernel never parses layout text.

use std::fs;

use slopos_keymap_core::{LayoutTable, SERIALIZED_LEN, parse, serialize};

use crate::syscall::keymap::{keymap_get_name, keymap_load};

/// Where the installed `*.layout` files live.
pub const KEYMAP_DIR: &str = "/usr/share/keymaps";

/// The persisted layout choice (the short name, e.g. `de_CH`), applied at boot.
pub const PERSIST_PATH: &str = "/etc/keymap";

pub fn current_name() -> Option<String> {
    let mut buf = [0u8; 16];
    let n = keymap_get_name(&mut buf);
    if n < 0 {
        return None;
    }
    let n = (n as usize).min(buf.len());
    core::str::from_utf8(&buf[..n]).ok().map(String::from)
}

/// Load `/usr/share/keymaps/<name>.layout`, parse and validate it, and upload
/// the binary table to the kernel. The `Err` names the failing stage.
pub fn load_layout_by_name(name: &str) -> Result<(), &'static str> {
    let path = format!("{KEYMAP_DIR}/{name}.layout");
    let src = fs::read(&path).map_err(|_| "no such layout")?;

    let mut table = Box::new(LayoutTable::empty());
    parse(&src, &mut table).map_err(|_| "failed to parse layout")?;
    let mut blob = vec![0u8; SERIALIZED_LEN];
    serialize(&table, &mut blob).map_err(|_| "failed to serialise layout")?;

    if keymap_load(&blob) < 0 {
        return Err("kernel rejected layout");
    }
    Ok(())
}

/// Apply the layout name persisted in [`PERSIST_PATH`], if any. Best-effort and
/// silent: any failure leaves the built-in default active.
pub fn apply_persisted_layout() {
    let Ok(bytes) = fs::read(PERSIST_PATH) else {
        return;
    };
    let Ok(text) = core::str::from_utf8(&bytes) else {
        return;
    };
    let name = text.trim();
    if name.is_empty() {
        return;
    }
    let _ = load_layout_by_name(name);
}
