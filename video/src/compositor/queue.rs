//! Event Queue for Compositor
//!
//! Provides a message queue for compositor events. Events are enqueued
//! from various contexts (syscalls, task cleanup) and processed sequentially
//! by the compositor's event loop.

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use super::events::CompositorEvent;

/// Maximum number of events that can be queued
const DEFAULT_QUEUE_CAPACITY: usize = 256;

/// Event queue for compositor messages.
///
/// Uses a spin mutex only for the brief moment of enqueue/dequeue.
/// The compositor owns the drain operation, ensuring single-threaded processing.
pub struct EventQueue {
    /// Events pending dispatch
    events: Mutex<VecDeque<CompositorEvent>>,
    /// Flag to signal pending events (for efficient polling)
    pending: AtomicBool,
    /// Maximum queue capacity
    capacity: usize,
}

impl EventQueue {
    /// Create a new event queue with default capacity
    pub const fn new() -> Self {
        Self::with_capacity(DEFAULT_QUEUE_CAPACITY)
    }

    /// Create a new event queue with specified capacity
    pub const fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            pending: AtomicBool::new(false),
            capacity,
        }
    }

    /// Enqueue an event (called from any context).
    ///
    /// Returns `true` if the event was successfully queued,
    /// `false` if the queue is full.
    pub fn enqueue(&self, event: CompositorEvent) -> bool {
        let mut guard = self.events.lock();
        if guard.len() >= self.capacity {
            return false;
        }
        guard.push_back(event);
        self.pending.store(true, Ordering::Release);
        true
    }

    /// Drain all pending events (called only by compositor).
    ///
    /// Returns events in FIFO order. After draining, the queue is empty.
    pub fn drain(&self) -> VecDeque<CompositorEvent> {
        self.pending.store(false, Ordering::Release);
        let mut guard = self.events.lock();
        core::mem::take(&mut *guard)
    }

    /// Check if there are pending events (lock-free).
    ///
    /// This is a hint - the actual state may change between check and drain.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Get the current number of queued events.
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests would go here in a std environment
}
