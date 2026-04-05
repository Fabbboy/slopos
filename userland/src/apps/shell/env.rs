//! Environment variable storage for the shell.

use std::sync::Mutex;

pub const MAX_ENV_ENTRIES: usize = 64;
pub const ENV_KEY_MAX: usize = 64;
pub const ENV_VALUE_MAX: usize = 256;

#[derive(Clone, Copy)]
struct EnvEntry {
    key: [u8; ENV_KEY_MAX],
    value: [u8; ENV_VALUE_MAX],
    key_len: u8,
    value_len: u16,
    active: bool,
}

impl EnvEntry {
    const fn empty() -> Self {
        Self {
            key: [0; ENV_KEY_MAX],
            value: [0; ENV_VALUE_MAX],
            key_len: 0,
            value_len: 0,
            active: false,
        }
    }
}

struct Environment {
    entries: [EnvEntry; MAX_ENV_ENTRIES],
}

impl Environment {
    const fn new() -> Self {
        Self {
            entries: [EnvEntry::empty(); MAX_ENV_ENTRIES],
        }
    }
}

static ENV: Mutex<Environment> = Mutex::new(Environment::new());

fn with_env<R, F: FnOnce(&mut Environment) -> R>(f: F) -> R {
    f(&mut ENV.lock().unwrap())
}

fn key_matches(entry: &EnvEntry, key: &[u8]) -> bool {
    let klen = entry.key_len as usize;
    if klen != key.len() {
        return false;
    }
    entry.key[..klen] == *key
}

pub fn get(key: &[u8]) -> Option<([u8; ENV_VALUE_MAX], usize)> {
    with_env(|env| {
        for entry in &env.entries {
            if entry.active && key_matches(entry, key) {
                let mut value = [0u8; ENV_VALUE_MAX];
                let len = entry.value_len as usize;
                value[..len].copy_from_slice(&entry.value[..len]);
                return Some((value, len));
            }
        }
        None
    })
}

pub fn get_into(key: &[u8], dst: &mut [u8]) -> Option<usize> {
    with_env(|env| {
        for entry in &env.entries {
            if entry.active && key_matches(entry, key) {
                let len = (entry.value_len as usize).min(dst.len());
                dst[..len].copy_from_slice(&entry.value[..len]);
                return Some(len);
            }
        }
        None
    })
}

pub fn set(key: &[u8], value: &[u8]) {
    if key.is_empty() || key.len() > ENV_KEY_MAX {
        return;
    }
    with_env(|env| {
        for entry in &mut env.entries {
            if entry.active && key_matches(entry, key) {
                let vlen = value.len().min(ENV_VALUE_MAX);
                entry.value = [0; ENV_VALUE_MAX];
                entry.value[..vlen].copy_from_slice(&value[..vlen]);
                entry.value_len = vlen as u16;
                return;
            }
        }
        for entry in &mut env.entries {
            if !entry.active {
                let klen = key.len().min(ENV_KEY_MAX);
                entry.key = [0; ENV_KEY_MAX];
                entry.key[..klen].copy_from_slice(&key[..klen]);
                entry.key_len = klen as u8;
                let vlen = value.len().min(ENV_VALUE_MAX);
                entry.value = [0; ENV_VALUE_MAX];
                entry.value[..vlen].copy_from_slice(&value[..vlen]);
                entry.value_len = vlen as u16;
                entry.active = true;
                return;
            }
        }
    });
}

pub fn unset(key: &[u8]) -> bool {
    with_env(|env| {
        for entry in &mut env.entries {
            if entry.active && key_matches(entry, key) {
                *entry = EnvEntry::empty();
                return true;
            }
        }
        false
    })
}

pub fn initialize_defaults() {
    set(b"PATH", b"/bin:/sbin");
    set(b"SHELL", b"/bin/shell");
    set(b"HOME", b"/");
    set(b"USER", b"root");
    set(b"TERM", b"slopos");
    set(b"PS1", b"\\u@\\h:\\w\\$ ");
}

pub fn for_each<F: FnMut(&[u8], &[u8])>(mut f: F) {
    with_env(|env| {
        for entry in &env.entries {
            if entry.active {
                f(
                    &entry.key[..entry.key_len as usize],
                    &entry.value[..entry.value_len as usize],
                );
            }
        }
    });
}

pub fn count() -> usize {
    with_env(|env| env.entries.iter().filter(|e| e.active).count())
}
