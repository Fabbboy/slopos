//! Client surface cache for shared memory mappings.
//!
//! Maps client surface buffers read-only so the compositor can composite window
//! contents without re-mapping every frame.
//!
//! Each entry is keyed by `(task_id, generation, buffer_id)`. `buffer_id` is the
//! double-buffer slot — not the recyclable fd number — so a surface's ping-pong
//! between its two slots keeps both mappings alive without thrashing.
//! `generation` is the surface's incarnation id; since `task_id` is a recyclable
//! surface-slot index, the generation is what stops a recycled slot from
//! matching a prior surface's entry and compositing its pixels.
//!
//! The mapping only **borrows** the fd: the protocol bridge owns every buffer fd
//! and is its sole closer. The cache `munmap`s on eviction/teardown but never
//! closes the fd. A `MAP_SHARED` mapping stays valid after its fd is closed, so a
//! borrowed mapping outliving the fd is safe.

use crate::syscall::{CachedShmMapping, UserWindowInfo};

use super::MAX_WINDOWS;

/// Double-buffer slots cached per task.
const BUFFERS_PER_TASK: usize = 2;
const CACHE_SLOTS: usize = MAX_WINDOWS * BUFFERS_PER_TASK;

/// Single cache entry, keyed by `(task_id, generation, buffer_id)`; `token` is
/// the fd the mapping currently covers (detects a slot re-registered with a new
/// fd within the same incarnation, e.g. on resize).
struct ClientSurfaceEntry {
    task_id: u32,
    generation: u32,
    buffer_id: u8,
    token: u32,
    mapping: Option<CachedShmMapping>,
}

impl ClientSurfaceEntry {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            generation: 0,
            buffer_id: 0,
            token: 0,
            mapping: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.mapping.is_none()
    }

    fn keyed(&self, task_id: u32, generation: u32, buffer_id: u8) -> bool {
        self.mapping.is_some()
            && self.task_id == task_id
            && self.generation == generation
            && self.buffer_id == buffer_id
    }
}

/// Cache of mapped client surfaces (100% safe — no raw pointers).
pub struct ClientSurfaceCache {
    entries: [ClientSurfaceEntry; CACHE_SLOTS],
}

impl ClientSurfaceCache {
    pub fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| ClientSurfaceEntry::empty()),
        }
    }

    /// Get or create a cache index for the `(task_id, buffer_id)` buffer backed
    /// by fd `token`. Returns `None` when the token is zero, no slot is free, or
    /// the mapping fails. If the slot exists but its fd changed (re-registration
    /// on resize), the old mapping is dropped (munmap; the bridge closes the fd)
    /// and the new fd mapped in place.
    pub fn get_or_create_index(
        &mut self,
        task_id: u32,
        generation: u32,
        buffer_id: u8,
        token: u32,
        buffer_size: usize,
    ) -> Option<usize> {
        if token == 0 {
            return None;
        }

        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.keyed(task_id, generation, buffer_id))
        {
            if self.entries[i].token == token {
                return Some(i);
            }
            // Slot re-registered with a new fd (same incarnation, e.g. resize):
            // re-map in place (drop munmaps the stale borrowed mapping; the
            // bridge owns/closes the fd).
            let mapping = CachedShmMapping::map_readonly_fd_borrowed(token as i32, buffer_size)?;
            self.entries[i].token = token;
            self.entries[i].mapping = Some(mapping);
            return Some(i);
        }

        let slot = self.entries.iter().position(|e| e.is_empty())?;
        let mapping = CachedShmMapping::map_readonly_fd_borrowed(token as i32, buffer_size)?;
        self.entries[slot] = ClientSurfaceEntry {
            task_id,
            generation,
            buffer_id,
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

    /// Drop mappings for surface incarnations that no longer appear in the
    /// current window list. Matching on `(task_id, generation)` — not `task_id`
    /// alone — means a recycled slot whose generation has advanced evicts the
    /// prior incarnation's entries, so a stale mapping is never carried into a
    /// new surface. Both buffer slots of a still-present incarnation are kept
    /// (the window only exports its *current* buffer_id, so this is deliberately
    /// generation- not buffer_id-scoped to avoid evicting the idle slot).
    pub fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.is_empty() {
                continue;
            }
            let still_present = (0..window_count as usize).any(|i| {
                windows[i].task_id == entry.task_id
                    && windows[i].buffer_generation == entry.generation
            });
            if !still_present {
                // CachedShmMapping::drop munmaps; the fd is the bridge's to close.
                *entry = ClientSurfaceEntry::empty();
            }
        }
    }
}
