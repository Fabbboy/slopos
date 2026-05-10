//! Non-atomic, fixed-size bitmap over `[usize; W]` words.
//!
//! `W` is the number of machine words.  Use [`words_for`] to compute it:
//! `Bitmap<{ words_for(16_384) }>` for a 16 384-bit bitmap.
//!
//! Query methods accept `nbits` — the count of *valid* bits (≤ [`Bitmap::CAPACITY`]).
//! Bits beyond `nbits` in the last word are masked out automatically.

const WORD_BITS: usize = usize::BITS as usize;

/// Number of `usize` words needed to store `bits` bits.
#[inline]
pub const fn words_for(bits: usize) -> usize {
    (bits + WORD_BITS - 1) / WORD_BITS
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

/// Fixed-size, non-atomic bitmap.  Bit 0 is the LSB of `words[0]`.
///
/// Mutation requires `&mut self`; external synchronisation is the caller's
/// responsibility.
#[derive(Clone)]
pub struct Bitmap<const W: usize> {
    words: [usize; W],
}

// SAFETY: `Bitmap<W>` is a single `[usize; W]` field. `usize` is
// `Zeroable` and arrays of `Zeroable` are `Zeroable`; the all-zero
// pattern is the empty bitmap, the same value `Bitmap::new` returns.
// This unlocks `KBox::<Bitmap<W>>::zeroed()` so callers wrapping a
// large bitmap can heap-allocate it without the W-word stack temp
// that `let b = Bitmap::new()` materialises.
unsafe impl<const W: usize> slopos_ostd::mm::init::Zeroable for Bitmap<W> {}

impl<const W: usize> Bitmap<W> {
    pub const CAPACITY: usize = W * WORD_BITS;

    #[inline]
    pub const fn new() -> Self {
        Self { words: [0; W] }
    }

    #[inline]
    pub const fn new_full() -> Self {
        Self {
            words: [usize::MAX; W],
        }
    }

    #[inline]
    pub fn test(&self, bit: usize) -> bool {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        (self.words[word] & mask) != 0
    }

    #[inline]
    pub fn set(&mut self, bit: usize) {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        self.words[word] |= mask;
    }

    #[inline]
    pub fn clear(&mut self, bit: usize) {
        debug_assert!(bit < Self::CAPACITY);
        let (word, mask) = Self::word_mask(bit);
        self.words[word] &= !mask;
    }

    /// Set contiguous range `[start .. start+len)`.
    pub fn set_range(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        debug_assert!(start + len <= Self::CAPACITY);

        let mut bit = start;
        let end = start + len;
        while bit < end {
            let word_idx = bit / WORD_BITS;
            let bit_in_word = bit % WORD_BITS;
            let bits_left = end - bit;
            let bits_this_word = (WORD_BITS - bit_in_word).min(bits_left);

            let mask = if bits_this_word == WORD_BITS {
                usize::MAX
            } else {
                ((1usize << bits_this_word) - 1) << bit_in_word
            };
            self.words[word_idx] |= mask;
            bit += bits_this_word;
        }
    }

    #[inline]
    pub fn clear_all(&mut self) {
        self.words = [0; W];
    }

    pub fn find_first_zero(&self, nbits: usize) -> Option<usize> {
        self.find_next_zero(0, nbits)
    }

    pub fn find_next_zero(&self, start: usize, nbits: usize) -> Option<usize> {
        debug_assert!(nbits <= Self::CAPACITY);
        if start >= nbits {
            return None;
        }

        let start_word = start / WORD_BITS;
        let last_word = (nbits - 1) / WORD_BITS;

        for i in start_word..=last_word.min(W - 1) {
            let mut inverted = !self.words[i];

            if i == start_word {
                inverted &= usize::MAX << (start % WORD_BITS);
            }
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

    pub fn find_first_set(&self, nbits: usize) -> Option<usize> {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return None;
        }
        let last_word = (nbits - 1) / WORD_BITS;

        for i in 0..=last_word.min(W - 1) {
            let mut word = self.words[i];
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

    pub fn count_ones(&self, nbits: usize) -> usize {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return 0;
        }
        let last_word = (nbits - 1) / WORD_BITS;
        let mut total = 0usize;
        for i in 0..last_word.min(W) {
            total += self.words[i].count_ones() as usize;
        }
        if last_word < W {
            total += (self.words[last_word] & last_word_mask(nbits)).count_ones() as usize;
        }
        total
    }

    pub fn is_empty(&self, nbits: usize) -> bool {
        self.count_ones(nbits) == 0
    }

    pub fn is_full(&self, nbits: usize) -> bool {
        debug_assert!(nbits <= Self::CAPACITY);
        if nbits == 0 {
            return true;
        }
        let last_word = (nbits - 1) / WORD_BITS;
        for i in 0..last_word.min(W) {
            if self.words[i] != usize::MAX {
                return false;
            }
        }
        if last_word < W {
            let mask = last_word_mask(nbits);
            if (self.words[last_word] & mask) != mask {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn iter_ones(&self, nbits: usize) -> IterOnes<'_, W> {
        debug_assert!(nbits <= Self::CAPACITY);
        IterOnes {
            bitmap: self,
            nbits,
            word_idx: 0,
            current: if W > 0 { self.words[0] } else { 0 },
        }
    }

    #[inline]
    pub fn iter_zeros(&self, nbits: usize) -> IterZeros<'_, W> {
        debug_assert!(nbits <= Self::CAPACITY);
        IterZeros {
            bitmap: self,
            nbits,
            word_idx: 0,
            current: if W > 0 { !self.words[0] } else { 0 },
        }
    }

    #[inline]
    pub fn load_word(&self, idx: usize) -> usize {
        self.words[idx]
    }

    #[inline]
    const fn word_mask(bit: usize) -> (usize, usize) {
        (bit / WORD_BITS, 1usize << (bit % WORD_BITS))
    }
}

pub struct IterOnes<'a, const W: usize> {
    bitmap: &'a Bitmap<W>,
    nbits: usize,
    word_idx: usize,
    current: usize,
}

impl<const W: usize> Iterator for IterOnes<'_, W> {
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
            self.current = self.bitmap.words[self.word_idx];
        }
    }
}

pub struct IterZeros<'a, const W: usize> {
    bitmap: &'a Bitmap<W>,
    nbits: usize,
    word_idx: usize,
    current: usize,
}

impl<const W: usize> Iterator for IterZeros<'_, W> {
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
            self.current = !self.bitmap.words[self.word_idx];
        }
    }
}
