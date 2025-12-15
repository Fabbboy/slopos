/// Simple fixed-capacity ring buffer mirroring the old C macros.
/// Uses a backing array with head/tail/count indices.
#[derive(Debug)]
pub struct RingBuffer<T, const N: usize> {
    data: [T; N],
    head: u32,
    tail: u32,
    count: u32,
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
    pub fn capacity(&self) -> u32 {
        N as u32
    }

    #[inline(always)]
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.count >= self.capacity()
    }

    /// Push with overwrite of the oldest element when full (like RING_BUFFER_PUSH_OVERWRITE).
    #[inline(always)]
    pub fn push_overwrite(&mut self, value: T) {
        if self.is_full() {
            self.tail = (self.tail + 1) % self.capacity();
            self.count -= 1;
        }
        self.data[self.head as usize] = value;
        self.head = (self.head + 1) % self.capacity();
        self.count += 1;
    }

    /// Push without overwrite; returns true on success, false if full.
    #[inline(always)]
    pub fn try_push(&mut self, value: T) -> bool {
        if self.is_full() {
            return false;
        }
        self.data[self.head as usize] = value;
        self.head = (self.head + 1) % self.capacity();
        self.count += 1;
        true
    }

    /// Pop oldest element; returns Some(value) or None when empty.
    #[inline(always)]
    pub fn try_pop(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let value = self.data[self.tail as usize];
        self.tail = (self.tail + 1) % self.capacity();
        self.count -= 1;
        Some(value)
    }

    /// Expose internal slice for debugging/testing.
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }
}
