//! Static command buffer management for the shell.

use std::sync::Mutex;

use crate::syscall::UserFsEntry;

pub const SHELL_PATH_BUF: usize = 128;
pub const EXPAND_BUF_SIZE: usize = 512;

static LINE_BUF: Mutex<[u8; 256]> = Mutex::new([0; 256]);

static EXPAND_BUF: Mutex<[u8; EXPAND_BUF_SIZE]> = Mutex::new([0; EXPAND_BUF_SIZE]);

static PATH_BUF: Mutex<[u8; SHELL_PATH_BUF]> = Mutex::new([0; SHELL_PATH_BUF]);

static LIST_ENTRIES: Mutex<[UserFsEntry; 32]> = Mutex::new([UserFsEntry::new(); 32]);

/// Parsed token storage: one byte arena plus a span per token, so neither the
/// number of words on a line nor the length of any one of them is capped.
pub struct ParsedTokens {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

impl ParsedTokens {
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            spans: Vec::new(),
        }
    }

    pub fn token(&self, idx: usize) -> &[u8] {
        let (start, end) = self.spans[idx];
        &self.bytes[start..end]
    }

    /// Append a token. Returns its index.
    pub fn push_token(&mut self, content: &[u8]) -> usize {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(content);
        self.spans.push((start, self.bytes.len()));
        self.spans.len() - 1
    }

    pub fn count(&self) -> usize {
        self.spans.len()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
    }
}

pub fn with_line_buf<R, F: FnOnce(&mut [u8; 256]) -> R>(f: F) -> R {
    f(&mut LINE_BUF.lock().unwrap())
}

pub fn with_expand_buf<R, F: FnOnce(&mut [u8; EXPAND_BUF_SIZE]) -> R>(f: F) -> R {
    f(&mut EXPAND_BUF.lock().unwrap())
}

pub fn with_path_buf<R, F: FnOnce(&mut [u8; SHELL_PATH_BUF]) -> R>(f: F) -> R {
    f(&mut PATH_BUF.lock().unwrap())
}

pub fn with_list_entries<R, F: FnOnce(&mut [UserFsEntry; 32]) -> R>(f: F) -> R {
    f(&mut LIST_ENTRIES.lock().unwrap())
}
