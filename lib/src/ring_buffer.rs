/// Simple fixed-capacity ring buffer mirroring the old C macros.
/// Uses a backing array with head/tail/count indices.
#[derive(Debug, Clone, Copy)]
pub struct RingBuffer<T, const N: usize> {
    data: [T; N],
    head: usize,
    tail: usize,
    count: usize,
}

impl<T: Copy, const N: usize> RingBuffer<T, N> {
    /// Create a new ring buffer with all elements set to the given value.
    /// This is const-compatible and can be used for static initialization.
    #[inline(always)]
    pub const fn new_with(value: T) -> Self {
        Self {
            data: [value; N],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Returns the current number of elements in the buffer.
    #[inline(always)]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        N
    }

    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.count >= N
    }

    #[inline(always)]
    pub fn free_space(&self) -> usize {
        N - self.count
    }

    /// Peek at the oldest element without removing it.
    #[inline(always)]
    pub fn peek(&self) -> Option<&T> {
        if self.is_empty() {
            return None;
        }
        Some(&self.data[self.tail])
    }

    /// Expose internal slice for debugging/testing.
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }
}

impl<T: Copy + Default, const N: usize> RingBuffer<T, N> {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            data: [T::default(); N],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    #[inline(always)]
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }

    /// Discard the oldest `n` elements without reading them.
    /// Saturates at the current element count.
    #[inline(always)]
    pub fn consume(&mut self, n: usize) {
        let consumed = n.min(self.count);
        self.tail = (self.tail + consumed) % N;
        self.count -= consumed;
    }

    /// Push with overwrite of the oldest element when full (like RING_BUFFER_PUSH_OVERWRITE).
    #[inline(always)]
    pub fn push_overwrite(&mut self, value: T) {
        if self.is_full() {
            self.tail = (self.tail + 1) % N;
            self.count -= 1;
        }
        self.data[self.head] = value;
        self.head = (self.head + 1) % N;
        self.count += 1;
    }

    /// Push without overwrite; returns true on success, false if full.
    #[inline(always)]
    pub fn try_push(&mut self, value: T) -> bool {
        if self.is_full() {
            return false;
        }
        self.data[self.head] = value;
        self.head = (self.head + 1) % N;
        self.count += 1;
        true
    }

    /// Pop oldest element; returns Some(value) or None when empty.
    #[inline(always)]
    pub fn try_pop(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let value = self.data[self.tail];
        self.tail = (self.tail + 1) % N;
        self.count -= 1;
        Some(value)
    }
}

/// Byte-stream bulk operations for protocols like TCP, serial, TTY.
///
/// Uses split `copy_from_slice` (compiles to memcpy) instead of
/// byte-by-byte loops for contiguous segments of the backing array.
impl<const N: usize> RingBuffer<u8, N> {
    #[inline(always)]
    pub const fn new_zeroed() -> Self {
        Self {
            data: [0u8; N],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Write up to `data.len()` bytes into the buffer.
    /// Returns the number of bytes actually written (capped at free space).
    pub fn write(&mut self, data: &[u8]) -> usize {
        let to_write = data.len().min(N - self.count);
        if to_write == 0 {
            return 0;
        }

        let first = to_write.min(N - self.head);
        self.data[self.head..self.head + first].copy_from_slice(&data[..first]);

        let second = to_write - first;
        if second > 0 {
            self.data[..second].copy_from_slice(&data[first..first + second]);
        }

        self.head = (self.head + to_write) % N;
        self.count += to_write;
        to_write
    }

    /// Read up to `out.len()` bytes from the buffer into `out`.
    /// Returns the number of bytes actually read.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let to_read = out.len().min(self.count);
        if to_read == 0 {
            return 0;
        }

        let first = to_read.min(N - self.tail);
        out[..first].copy_from_slice(&self.data[self.tail..self.tail + first]);

        let second = to_read - first;
        if second > 0 {
            out[first..first + second].copy_from_slice(&self.data[..second]);
        }

        self.tail = (self.tail + to_read) % N;
        self.count -= to_read;
        to_read
    }

    /// Peek at buffered data starting `offset` bytes from the tail,
    /// copying into `out`. Does not advance the tail.
    /// Returns the number of bytes copied.
    pub fn peek_at(&self, offset: usize, out: &mut [u8]) -> usize {
        if offset >= self.count {
            return 0;
        }
        let available = self.count - offset;
        let to_read = out.len().min(available);
        if to_read == 0 {
            return 0;
        }

        let start = (self.tail + offset) % N;
        let first = to_read.min(N - start);
        out[..first].copy_from_slice(&self.data[start..start + first]);

        let second = to_read - first;
        if second > 0 {
            out[first..first + second].copy_from_slice(&self.data[..second]);
        }

        to_read
    }
}
