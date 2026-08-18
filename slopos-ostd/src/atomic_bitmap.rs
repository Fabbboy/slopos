//! Atomic, fixed-size bitmap over `[AtomicUsize; W]` words.
//!
//! Query methods (`find_*`, `count_ones`) return **point-in-time snapshots**
//! that may be stale under contention; [`alloc`](AtomicBitmap::alloc) /
//! [`free`](AtomicBitmap::free) is the linearizable allocate-or-fail pair.

use core::sync::atomic::{AtomicUsize, Ordering};

const WORD_BITS: usize = usize::BITS as usize;

/// Fixed-size atomic bitmap.  Bit 0 is the LSB of `words[0]`.
pub struct AtomicBitmap<const W: usize> {
    words: [AtomicUsize; W],
}

// SAFETY: AtomicUsize is Send+Sync; the wrapper adds no non-Send/Sync state.
unsafe impl<const W: usize> Send for AtomicBitmap<W> {}
unsafe impl<const W: usize> Sync for AtomicBitmap<W> {}

impl<const W: usize> AtomicBitmap<W> {
    pub const CAPACITY: usize = W * WORD_BITS;

    #[inline]
    pub const fn new() -> Self {
        Self {
            words: [const { AtomicUsize::new(0) }; W],
        }
    }

    #[inline]
    pub fn test(&self, bit: usize) -> bool {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        (self.words[word].load(Ordering::Acquire) & mask) != 0
    }

    #[inline]
    pub fn set(&self, bit: usize) {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        self.words[word].fetch_or(mask, Ordering::Release);
    }

    #[inline]
    pub fn clear(&self, bit: usize) {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        self.words[word].fetch_and(!mask, Ordering::Release);
    }

    /// Atomically set `bit` and return whether it was previously clear.
    #[inline]
    pub fn test_and_set(&self, bit: usize) -> bool {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        (self.words[word].fetch_or(mask, Ordering::AcqRel) & mask) == 0
    }

    /// Atomically clear `bit` and return whether it was previously set.
    #[inline]
    pub fn test_and_clear(&self, bit: usize) -> bool {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        (self.words[word].fetch_and(!mask, Ordering::AcqRel) & mask) != 0
    }

    /// Alias for [`clear`](Self::clear).
    #[inline]
    pub fn free(&self, bit: usize) {
        self.clear(bit);
    }

    /// Lock-free find-first-zero-and-set over `[0 .. nbits)`; `None` if all set.
    pub fn alloc(&self, nbits: usize) -> Option<usize> {
        self.alloc_from(0, nbits)
    }

    /// Lock-free find-next-zero-and-set starting from `start`.
    pub fn alloc_from(&self, start: usize, nbits: usize) -> Option<usize> {
        debug_assert!(nbits <= Self::CAPACITY);
        if start >= nbits {
            return None;
        }

        let start_word = start / WORD_BITS;
        for word_idx in start_word..W {
            loop {
                let current = self.words[word_idx].load(Ordering::Relaxed);
                if current == usize::MAX {
                    break;
                }
                let mut inverted = !current;
                if word_idx == start_word {
                    inverted &= usize::MAX << (start % WORD_BITS);
                }
                if inverted == 0 {
                    break;
                }
                let bit = inverted.trailing_zeros() as usize;
                let abs_bit = word_idx * WORD_BITS + bit;
                if abs_bit >= nbits {
                    return None;
                }
                let new = current | (1usize << bit);
                if self.words[word_idx]
                    .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(abs_bit);
                }
            }
        }
        None
    }

    /// Snapshot: find the first zero bit in `[0 .. nbits)`.
    pub fn find_first_zero(&self, nbits: usize) -> Option<usize> {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return None;
        }
        let last_word = (nbits - 1) / WORD_BITS;
        for i in 0..=last_word.min(W - 1) {
            let mut inverted = !self.words[i].load(Ordering::Relaxed);
            if i == last_word {
                inverted &= last_word_mask(nbits);
            }
            if inverted == 0 {
                continue;
            }
            let bit = i * WORD_BITS + inverted.trailing_zeros() as usize;
            if bit < nbits {
                return Some(bit);
            }
            return None;
        }
        None
    }

    /// Snapshot: find the first set bit in `[0 .. nbits)`.
    pub fn find_first_set(&self, nbits: usize) -> Option<usize> {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return None;
        }
        let last_word = (nbits - 1) / WORD_BITS;
        for i in 0..=last_word.min(W - 1) {
            let mut word = self.words[i].load(Ordering::Relaxed);
            if i == last_word {
                word &= last_word_mask(nbits);
            }
            if word == 0 {
                continue;
            }
            let bit = i * WORD_BITS + word.trailing_zeros() as usize;
            if bit < nbits {
                return Some(bit);
            }
            return None;
        }
        None
    }

    /// Snapshot: count set bits in `[0 .. nbits)`.
    pub fn count_ones(&self, nbits: usize) -> usize {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return 0;
        }
        let last_word = (nbits - 1) / WORD_BITS;
        let mut total = 0usize;
        for i in 0..last_word.min(W) {
            total += self.words[i].load(Ordering::Relaxed).count_ones() as usize;
        }
        if last_word < W {
            total += (self.words[last_word].load(Ordering::Relaxed) & last_word_mask(nbits))
                .count_ones() as usize;
        }
        total
    }

    #[inline]
    pub fn load_word(&self, idx: usize) -> usize {
        self.words[idx].load(Ordering::Acquire)
    }

    /// Snapshot iterator over set-bit indices; later mutations are not visible.
    pub fn iter_ones_snapshot(&self, nbits: usize) -> IterOnesSnapshot<W> {
        debug_assert!(nbits <= Self::CAPACITY);
        let mut words = [0usize; W];
        for i in 0..W {
            words[i] = self.words[i].load(Ordering::Acquire);
        }
        IterOnesSnapshot {
            words,
            nbits,
            word_idx: 0,
            current: if W > 0 { words[0] } else { 0 },
        }
    }

    #[inline]
    const fn word_mask(bit: usize) -> (usize, usize) {
        (bit / WORD_BITS, 1usize << (bit % WORD_BITS))
    }
}

#[inline]
const fn last_word_mask(nbits: usize) -> usize {
    let rem = nbits % WORD_BITS;
    if rem == 0 {
        usize::MAX
    } else {
        (1usize << rem) - 1
    }
}

pub struct IterOnesSnapshot<const W: usize> {
    words: [usize; W],
    nbits: usize,
    word_idx: usize,
    current: usize,
}

impl<const W: usize> Iterator for IterOnesSnapshot<W> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let tz = self.current.trailing_zeros() as usize;
                let bit = self.word_idx * WORD_BITS + tz;
                if bit >= self.nbits {
                    return None;
                }
                self.current &= self.current - 1;
                return Some(bit);
            }
            self.word_idx += 1;
            if self.word_idx >= W {
                return None;
            }
            self.current = self.words[self.word_idx];
        }
    }
}
