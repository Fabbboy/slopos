//! Static command buffer management for the shell.

use std::sync::Mutex;

use crate::syscall::UserFsEntry;

use super::parser::{SHELL_MAX_TOKEN_LENGTH, SHELL_MAX_TOKENS};

pub const SHELL_PATH_BUF: usize = 128;
pub const EXPAND_BUF_SIZE: usize = 512;

static LINE_BUF: Mutex<[u8; 256]> = Mutex::new([0; 256]);

static EXPAND_BUF: Mutex<[u8; EXPAND_BUF_SIZE]> = Mutex::new([0; EXPAND_BUF_SIZE]);

static PATH_BUF: Mutex<[u8; SHELL_PATH_BUF]> = Mutex::new([0; SHELL_PATH_BUF]);

static LIST_ENTRIES: Mutex<[UserFsEntry; 32]> = Mutex::new([UserFsEntry::new(); 32]);

/// Parsed token storage: owns all token data as inline byte arrays.
pub struct ParsedTokens {
    data: [[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS],
    count: usize,
}

impl ParsedTokens {
    pub const fn new() -> Self {
        Self {
            data: [[0; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS],
            count: 0,
        }
    }

    /// Get a token as a byte slice (up to the null terminator).
    pub fn token(&self, idx: usize) -> &[u8] {
        let slot = &self.data[idx];
        let len = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
        &slot[..len]
    }

    /// Get a mutable reference to the raw slot at `idx`.
    pub fn slot_mut(&mut self, idx: usize) -> &mut [u8; SHELL_MAX_TOKEN_LENGTH] {
        &mut self.data[idx]
    }

    /// Write a token into the next slot. Returns the index written.
    pub fn push_token(&mut self, content: &[u8]) -> usize {
        let idx = self.count;
        let len = content.len().min(SHELL_MAX_TOKEN_LENGTH - 1);
        self.data[idx][..len].copy_from_slice(&content[..len]);
        self.data[idx][len] = 0;
        self.count += 1;
        idx
    }

    /// Increment count after manually writing into a slot.
    pub fn advance(&mut self) {
        self.count += 1;
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn clear(&mut self) {
        self.count = 0;
        for slot in &mut self.data {
            slot[0] = 0;
        }
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
