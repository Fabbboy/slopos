//! Client surface cache for shared memory mappings.
//!
//! Manages read-only mappings of client surface buffers so the compositor can
//! composite window contents without re-mapping every frame.

use crate::syscall::{CachedShmMapping, UserWindowInfo};

use super::MAX_WINDOWS;

/// Single cache entry for a mapped client surface.
struct ClientSurfaceEntry {
    task_id: u32,
    token: u32,
    mapping: Option<CachedShmMapping>,
}

impl ClientSurfaceEntry {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            token: 0,
            mapping: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.task_id == 0 && self.mapping.is_none()
    }

    fn matches(&self, task_id: u32, token: u32) -> bool {
        self.task_id == task_id && self.token == token && self.mapping.is_some()
    }
}

/// Cache of mapped client surfaces (100% safe — no raw pointers).
pub struct ClientSurfaceCache {
    entries: [ClientSurfaceEntry; MAX_WINDOWS],
}

impl ClientSurfaceCache {
    pub fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| ClientSurfaceEntry::empty()),
        }
    }

    /// Get or create a cache index for the given surface. Returns `None` when
    /// the token is zero, the cache is full, or the mapping fails.
    pub fn get_or_create_index(
        &mut self,
        task_id: u32,
        token: u32,
        buffer_size: usize,
    ) -> Option<usize> {
        if token == 0 {
            return None;
        }

        for (i, entry) in self.entries.iter().enumerate() {
            if entry.matches(task_id, token) {
                return Some(i);
            }
        }

        let slot = self.entries.iter().position(|e| e.is_empty())?;

        let mapping = CachedShmMapping::map_readonly_fd(token as i32, buffer_size)?;
        self.entries[slot] = ClientSurfaceEntry {
            task_id,
            token,
            mapping: Some(mapping),
        };
        Some(slot)
    }

    /// Get a slice view of the cached buffer at the given index.
    pub fn get_slice(&self, index: usize) -> Option<&[u8]> {
        self.entries
            .get(index)?
            .mapping
            .as_ref()
            .map(|m| m.as_slice())
    }

    /// Drop mappings for surfaces that no longer appear in the current window list.
    pub fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.task_id == 0 {
                continue;
            }

            let mut stale = true;
            for i in 0..window_count as usize {
                if windows[i].task_id == entry.task_id {
                    if windows[i].shm_token == entry.token {
                        stale = false;
                    }
                    break;
                }
            }

            if stale {
                // CachedShmMapping::drop handles munmap+close automatically.
                *entry = ClientSurfaceEntry::empty();
            }
        }
    }
}
