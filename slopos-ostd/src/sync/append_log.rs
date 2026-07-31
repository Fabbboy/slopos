//! Fixed-capacity append-only byte log.
//!
//! A `.bss`-resident buffer that many CPUs append to and one reads back,
//! used for the per-CPU klog capture rings the test harness installs. The
//! buffer, the length and the overflow counters move together under one
//! lock, so both ends are safe to call: readers cannot observe a length
//! that runs past the bytes actually written, and a writer cannot extend
//! the log while a reader holds a view of it.
//!
//! Reads take a closure rather than returning a slice. A returned
//! `&'static [u8]` would outlive the lock and re-open the race the lock
//! exists to close — the same reason [`crate::util::ptr_buf`]'s `with_*`
//! forms are the shape to reach for there.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Append-only byte log of `N` bytes.
pub struct AppendLog<const N: usize> {
    /// Bytes plus the count of live ones. Both under `lock`.
    inner: UnsafeCell<Inner<N>>,
    lock: AtomicBool,
    /// Bytes that did not fit. Read without the lock; monotonic per window.
    dropped: AtomicUsize,
}

struct Inner<const N: usize> {
    buf: [u8; N],
    len: usize,
}

// SAFETY: every access to `inner` goes through `with_locked`, which holds
// the `lock` spin flag for the whole borrow, so the `&mut` it hands out is
// exclusive across CPUs.
unsafe impl<const N: usize> Sync for AppendLog<N> {}

impl<const N: usize> Default for AppendLog<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> AppendLog<N> {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner {
                buf: [0u8; N],
                len: 0,
            }),
            lock: AtomicBool::new(false),
            dropped: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn with_locked<R>(&self, f: impl FnOnce(&mut Inner<N>) -> R) -> R {
        while self.lock.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // SAFETY: the swap above transitioned this CPU from "unlocked" to
        // "locked"; every other accessor spins until the store below, so
        // this borrow is exclusive for its whole extent.
        let result = f(unsafe { &mut *self.inner.get() });
        self.lock.store(false, Ordering::Release);
        result
    }

    /// Discard the log's contents and its overflow count.
    pub fn reset(&self) {
        self.with_locked(|inner| inner.len = 0);
        self.dropped.store(0, Ordering::Relaxed);
    }

    /// Append what fits; count the rest as dropped.
    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let dropped = self.with_locked(|inner| {
            let take = bytes.len().min(N - inner.len);
            inner.buf[inner.len..inner.len + take].copy_from_slice(&bytes[..take]);
            inner.len += take;
            bytes.len() - take
        });
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Read the live bytes under the lock.
    ///
    /// `f` runs with appends from every CPU blocked, so it should not do
    /// unbounded work — and must not append to this same log, which would
    /// deadlock.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.with_locked(|inner| f(&inner.buf[..inner.len]))
    }

    /// Number of live bytes.
    pub fn len(&self) -> usize {
        self.with_locked(|inner| inner.len)
    }

    /// Whether the log holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes lost to overflow since the last [`AppendLog::reset`].
    pub fn dropped_bytes(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}
