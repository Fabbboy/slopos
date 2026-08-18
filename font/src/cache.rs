//! LRU glyph cache for avoiding re-rasterization of frequently used glyphs.

use slopos_ostd::KVec;

use crate::rasterizer::RasterizedGlyph;

const MAX_CACHE_SIZE: usize = 512;

/// Cache key: (codepoint, size_px).
type CacheKey = (u32, u16);

struct CacheEntry {
    key: CacheKey,
    glyph: RasterizedGlyph,
    access_count: u64,
}

/// A fixed-capacity LRU glyph cache.
pub struct GlyphCache {
    entries: KVec<CacheEntry>,
    access_counter: u64,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self {
            entries: KVec::with_capacity(64).expect("GlyphCache: alloc"),
            access_counter: 0,
        }
    }

    pub fn get(&mut self, codepoint: u32, size_px: u16) -> Option<&RasterizedGlyph> {
        let key = (codepoint, size_px);
        self.access_counter += 1;

        for entry in self.entries.iter_mut() {
            if entry.key == key {
                entry.access_count = self.access_counter;
                // Return a reference — need to re-search for borrow checker
                break;
            }
        }

        for entry in self.entries.iter() {
            if entry.key == key {
                return Some(&entry.glyph);
            }
        }

        None
    }

    /// Insert a rasterized glyph into the cache, evicting the LRU entry if full.
    pub fn insert(&mut self, codepoint: u32, size_px: u16, glyph: RasterizedGlyph) {
        let key = (codepoint, size_px);
        self.access_counter += 1;

        for entry in self.entries.iter_mut() {
            if entry.key == key {
                entry.glyph = glyph;
                entry.access_count = self.access_counter;
                return;
            }
        }

        if self.entries.len() >= MAX_CACHE_SIZE {
            let mut min_access = u64::MAX;
            let mut min_idx = 0;
            for (i, entry) in self.entries.iter().enumerate() {
                if entry.access_count < min_access {
                    min_access = entry.access_count;
                    min_idx = i;
                }
            }
            self.entries.swap_remove(min_idx);
        }

        let _ = self.entries.push(CacheEntry {
            key,
            glyph,
            access_count: self.access_counter,
        });
    }
}
