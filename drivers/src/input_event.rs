//! Input Event Protocol — per-task queues with SeqLock focus tracking.
//!
//! # Locking architecture
//!
//! Focus state (which task receives input) is protected by a [`SeqLock`] —
//! ISR handlers read it lock-free, the compositor writes it rarely.
//!
//! Per-task event queues are individually locked with [`IrqMutex`], so
//! event delivery to one task never blocks delivery to another.
//!
//! Queue slot allocation uses a small global [`IrqMutex`] touched only
//! on task creation/destruction.

use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SeqLock};
use slopos_utils::RingBuffer;

/// Monotonic millisecond timestamp for input events.
pub fn get_timestamp_ms() -> u64 {
    crate::hpet::nanoseconds(crate::hpet::read_counter()) / 1_000_000
}

pub use slopos_abi::{
    InputEvent, InputEventData, InputEventType, MAX_EVENTS_PER_TASK, MAX_INPUT_TASKS,
};

// =============================================================================
// Per-Task Event Queue (individually locked)
// =============================================================================

struct TaskEventQueue {
    task_id: u32,
    active: bool,
    events: RingBuffer<InputEvent, MAX_EVENTS_PER_TASK>,
}

impl TaskEventQueue {
    const fn new() -> Self {
        Self {
            task_id: 0,
            active: false,
            events: RingBuffer::new_with(InputEvent {
                event_type: InputEventType::KeyPress,
                _padding: [0; 3],
                timestamp_ms: 0,
                data: InputEventData { data0: 0, data1: 0 },
            }),
        }
    }
}

/// Per-task event queues — each independently locked.
static TASK_QUEUES: [IrqMutex<TaskEventQueue>; MAX_INPUT_TASKS] =
    [const { IrqMutex::new(TaskEventQueue::new(), LOCK_LEVEL_RESOURCE) }; MAX_INPUT_TASKS];

// =============================================================================
// Focus State (SeqLock — lock-free reads from ISR, rare writes by compositor)
// =============================================================================

/// Focus state — written rarely (focus change, compositor registration).
/// Protected by SeqLock for lock-free reads.
#[derive(Clone, Copy)]
struct InputFocusState {
    keyboard_focus: u32,
    pointer_focus: u32,
    window_offset_x: i32,
    window_offset_y: i32,
    compositor_task_id: u32,
}

impl InputFocusState {
    const fn new() -> Self {
        Self {
            keyboard_focus: 0,
            pointer_focus: 0,
            window_offset_x: 0,
            window_offset_y: 0,
            compositor_task_id: 0,
        }
    }
}

static FOCUS: SeqLock<InputFocusState> = SeqLock::new(InputFocusState::new());

/// Fast-changing pointer state — updated on every mouse event (ISR, up to
/// 1000Hz). Uses atomics instead of SeqLock to avoid writer contention in
/// the ISR path. These are NOT inside the SeqLock because a SeqLock write
/// on every mouse event would be excessive.
static POINTER_X: AtomicI32 = AtomicI32::new(0);
static POINTER_Y: AtomicI32 = AtomicI32::new(0);
static POINTER_BUTTONS: AtomicU8 = AtomicU8::new(0);

/// Fast atomic for has_keyboard_focus() — avoids SeqLock read for simple check.
static KEYBOARD_FOCUS_FAST: AtomicU32 = AtomicU32::new(0);

// =============================================================================
// Queue Slot Allocation (global lock, rare — only task create/destroy)
// =============================================================================

/// Lookup table size. Must cover any possible task_id. Using 16384 covers
/// all plausible IDs with negligible memory (64 KiB for u32 entries).
const TASK_MAP_SIZE: usize = 16384;

struct QueueAllocState {
    /// Maps task_id → queue slot index. 0 = unmapped.
    /// Index is stored as slot + 1 (so 0 means "not mapped").
    task_to_slot: [u32; TASK_MAP_SIZE],
}

impl QueueAllocState {
    const fn new() -> Self {
        Self {
            task_to_slot: [0u32; TASK_MAP_SIZE],
        }
    }
}

static QUEUE_ALLOC: IrqMutex<QueueAllocState> =
    IrqMutex::new(QueueAllocState::new(), LOCK_LEVEL_REGISTRY);

/// Lock-free slot allocation bitmap. Bit i = 1 means slot i is occupied.
/// Avoids holding QUEUE_ALLOC (L2) while locking TASK_QUEUES (L1).
static SLOT_BITMAP: AtomicU64 = AtomicU64::new(0);

/// Atomically claim a free slot from the bitmap. Returns slot index.
fn atomic_alloc_slot() -> Option<usize> {
    loop {
        let bits = SLOT_BITMAP.load(Ordering::Relaxed);
        let free = !bits;
        if free == 0 {
            return None; // All slots occupied
        }
        let slot = free.trailing_zeros() as usize;
        if slot >= MAX_INPUT_TASKS {
            return None;
        }
        let mask = 1u64 << slot;
        if SLOT_BITMAP
            .compare_exchange_weak(bits, bits | mask, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(slot);
        }
    }
}

/// Release a slot in the bitmap.
fn atomic_free_slot(slot: usize) {
    if slot < MAX_INPUT_TASKS {
        SLOT_BITMAP.fetch_and(!(1u64 << slot), Ordering::Release);
    }
}

/// Find or create a queue slot for a task. Returns slot index.
///
/// Uses a lock-free bitmap for slot allocation to avoid nesting
/// QUEUE_ALLOC (L2) inside TASK_QUEUES (L1). Locks are acquired
/// in ascending order only: L1 (TASK_QUEUES) then L2 (QUEUE_ALLOC).
fn resolve_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 || task_id as usize >= TASK_MAP_SIZE {
        return None;
    }

    // Fast path: check lookup table under QUEUE_ALLOC (L2 only).
    {
        let alloc = QUEUE_ALLOC.lock();
        let mapped = alloc.task_to_slot[task_id as usize];
        if mapped != 0 {
            return Some((mapped - 1) as usize);
        }
    }

    // Slow path: claim a free slot via atomic bitmap (no locks held).
    let slot = atomic_alloc_slot()?;

    // Initialize the slot (L1 only).
    {
        let mut queue = TASK_QUEUES[slot].lock();
        queue.task_id = task_id;
        queue.active = true;
        queue.events.reset();
    }

    // Register in lookup table (L2 only — L1 already released = ascending order).
    {
        let mut alloc = QUEUE_ALLOC.lock();
        // Double-check: another CPU may have raced and registered this task.
        let mapped = alloc.task_to_slot[task_id as usize];
        if mapped != 0 {
            // Lost the race — undo our slot claim.
            {
                let mut queue = TASK_QUEUES[slot].lock();
                queue.active = false;
                queue.task_id = 0;
            }
            atomic_free_slot(slot);
            return Some((mapped - 1) as usize);
        }
        alloc.task_to_slot[task_id as usize] = (slot + 1) as u32;
    }

    Some(slot)
}

/// Find an existing queue slot for a task. Returns slot index.
fn find_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 || task_id as usize >= TASK_MAP_SIZE {
        return None;
    }
    let alloc = QUEUE_ALLOC.lock();
    let mapped = alloc.task_to_slot[task_id as usize];
    if mapped != 0 {
        Some((mapped - 1) as usize)
    } else {
        None
    }
}

// =============================================================================
// Internal: push event to a task's queue by slot index
// =============================================================================

#[inline]
fn push_event(slot: usize, event: InputEvent) {
    if slot < MAX_INPUT_TASKS {
        let mut queue = TASK_QUEUES[slot].lock();
        if queue.active {
            queue.events.push_overwrite(event);
        }
    }
}

// =============================================================================
// Public API - Focus Management (Compositor Operations)
// =============================================================================

#[inline]
pub fn has_keyboard_focus() -> bool {
    KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) != 0
}

pub fn input_set_keyboard_focus(task_id: u32) {
    KEYBOARD_FOCUS_FAST.store(task_id, Ordering::Release);
    let mut guard = FOCUS.write_lock();
    guard.get_mut().keyboard_focus = task_id;
}

pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    input_set_pointer_focus_with_offset(task_id, 0, 0, timestamp_ms);
}

pub fn input_set_pointer_focus_with_offset(
    task_id: u32,
    offset_x: i32,
    offset_y: i32,
    timestamp_ms: u64,
) {
    let old_state = FOCUS.read();
    let old_focus = old_state.pointer_focus;
    let x = POINTER_X.load(Ordering::Relaxed);
    let y = POINTER_Y.load(Ordering::Relaxed);

    {
        let mut guard = FOCUS.write_lock();
        let s = guard.get_mut();
        s.pointer_focus = task_id;
        s.window_offset_x = offset_x;
        s.window_offset_y = offset_y;
    }

    if old_focus == task_id {
        return;
    }

    // Send leave event to old focus.
    if old_focus != 0 {
        if let Some(slot) = find_queue(old_focus) {
            push_event(
                slot,
                InputEvent::pointer_enter_leave(false, x, y, timestamp_ms),
            );
        }
    }

    // Send enter event to new focus (translated coords).
    if task_id != 0 {
        if let Some(slot) = resolve_queue(task_id) {
            let local_x = x - offset_x;
            let local_y = y - offset_y;
            push_event(
                slot,
                InputEvent::pointer_enter_leave(true, local_x, local_y, timestamp_ms),
            );
        }
    }
}

pub fn input_request_close(task_id: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    if let Some(slot) = resolve_queue(task_id) {
        push_event(slot, InputEvent::close_request(timestamp_ms));
        true
    } else {
        false
    }
}

pub fn input_send_configure(task_id: u32, width: u32, height: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    if let Some(slot) = resolve_queue(task_id) {
        push_event(slot, InputEvent::configure(width, height, timestamp_ms));
        true
    } else {
        false
    }
}

pub fn input_get_keyboard_focus() -> u32 {
    FOCUS.read().keyboard_focus
}

pub fn input_get_pointer_focus() -> u32 {
    FOCUS.read().pointer_focus
}

pub fn input_get_pointer_position() -> (i32, i32) {
    (
        POINTER_X.load(Ordering::Relaxed),
        POINTER_Y.load(Ordering::Relaxed),
    )
}

pub fn input_get_button_state() -> u8 {
    POINTER_BUTTONS.load(Ordering::Relaxed)
}

pub fn input_get_modifier_state() -> u8 {
    crate::ps2::keyboard::get_modifier_state()
}

// =============================================================================
// Public API - Event Routing (Called from IRQ handlers)
// =============================================================================

pub fn input_route_key_event(scancode: u8, ascii: u8, pressed: bool, timestamp_ms: u64) {
    let state = FOCUS.read();

    let target = if state.compositor_task_id != 0 {
        state.compositor_task_id
    } else {
        if KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) == 0 {
            return;
        }
        if state.keyboard_focus == 0 {
            return;
        }
        state.keyboard_focus
    };

    if let Some(slot) = resolve_queue(target) {
        let event_type = if pressed {
            InputEventType::KeyPress
        } else {
            InputEventType::KeyRelease
        };
        push_event(
            slot,
            InputEvent::key(event_type, scancode, ascii, timestamp_ms),
        );
    }
}

pub fn input_route_pointer_motion(x: i32, y: i32, timestamp_ms: u64) {
    // Update pointer position via atomics — no SeqLock write needed.
    POINTER_X.store(x, Ordering::Relaxed);
    POINTER_Y.store(y, Ordering::Relaxed);

    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = resolve_queue(comp_id) {
            push_event(slot, InputEvent::pointer_motion(x, y, timestamp_ms));
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }

    let local_x = x - state.window_offset_x;
    let local_y = y - state.window_offset_y;
    if let Some(slot) = resolve_queue(focus) {
        push_event(
            slot,
            InputEvent::pointer_motion(local_x, local_y, timestamp_ms),
        );
    }
}

pub fn input_route_pointer_button(button: u8, pressed: bool, timestamp_ms: u64) {
    // Update button state via atomic — no SeqLock write needed.
    if pressed {
        POINTER_BUTTONS.fetch_or(button, Ordering::Relaxed);
    } else {
        POINTER_BUTTONS.fetch_and(!button, Ordering::Relaxed);
    }

    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = resolve_queue(comp_id) {
            push_event(
                slot,
                InputEvent::pointer_button(pressed, button, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = resolve_queue(focus) {
        push_event(
            slot,
            InputEvent::pointer_button(pressed, button, timestamp_ms),
        );
    }
}

pub fn input_route_pointer_axis(axis: u32, value_v120: i32, timestamp_ms: u64) {
    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = resolve_queue(comp_id) {
            push_event(
                slot,
                InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = resolve_queue(focus) {
        push_event(
            slot,
            InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
        );
    }
}

// =============================================================================
// Public API - Compositor Registration
// =============================================================================

pub fn input_register_compositor(task_id: u32) {
    // Pre-create queue for compositor.
    let _ = resolve_queue(task_id);
    // Update focus state to route all raw input to compositor.
    let mut guard = FOCUS.write_lock();
    guard.get_mut().compositor_task_id = task_id;
}

// =============================================================================
// Public API - Client Operations (Syscalls)
// =============================================================================

pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    let slot = find_queue(task_id)?;
    let mut queue = TASK_QUEUES[slot].lock();
    queue.events.try_pop()
}

pub fn input_drain_batch(task_id: u32, out_buffer: *mut InputEvent, max_count: usize) -> usize {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let slot = match resolve_queue(task_id) {
        Some(s) => s,
        None => return 0,
    };

    let mut queue = TASK_QUEUES[slot].lock();
    let mut count = 0;
    while count < max_count {
        if let Some(event) = queue.events.try_pop() {
            unsafe {
                out_buffer.add(count).write(event);
            }
            count += 1;
        } else {
            break;
        }
    }
    count
}

pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    let slot = find_queue(task_id)?;
    let queue = TASK_QUEUES[slot].lock();
    queue.events.peek().copied()
}

pub fn input_has_events(task_id: u32) -> bool {
    let slot = match find_queue(task_id) {
        Some(s) => s,
        None => return false,
    };
    let queue = TASK_QUEUES[slot].lock();
    !queue.events.is_empty()
}

pub fn input_event_count(task_id: u32) -> u32 {
    let slot = match find_queue(task_id) {
        Some(s) => s,
        None => return 0,
    };
    let queue = TASK_QUEUES[slot].lock();
    queue.events.len() as u32
}

// =============================================================================
// Clipboard (unchanged — separate lock, already fine)
// =============================================================================

struct ClipboardState {
    data: [u8; slopos_abi::CLIPBOARD_MAX_SIZE],
    len: usize,
}

impl ClipboardState {
    const fn new() -> Self {
        Self {
            data: [0u8; slopos_abi::CLIPBOARD_MAX_SIZE],
            len: 0,
        }
    }
}

static CLIPBOARD: IrqMutex<ClipboardState> =
    IrqMutex::new(ClipboardState::new(), LOCK_LEVEL_RESOURCE);

pub fn clipboard_copy(src: &[u8]) -> usize {
    let mut clip = CLIPBOARD.lock();
    let copy_len = src.len().min(slopos_abi::CLIPBOARD_MAX_SIZE);
    clip.data[..copy_len].copy_from_slice(&src[..copy_len]);
    clip.len = copy_len;
    copy_len
}

pub fn clipboard_paste(dst: &mut [u8]) -> usize {
    let clip = CLIPBOARD.lock();
    if clip.len == 0 {
        return 0;
    }
    let copy_len = clip.len.min(dst.len());
    dst[..copy_len].copy_from_slice(&clip.data[..copy_len]);
    copy_len
}

// =============================================================================
// Task Cleanup
// =============================================================================

pub fn input_cleanup_task(task_id: u32) {
    // Clear focus if this task had it.
    {
        let current = FOCUS.read();
        if current.keyboard_focus == task_id || current.pointer_focus == task_id {
            let mut guard = FOCUS.write_lock();
            let s = guard.get_mut();
            if s.keyboard_focus == task_id {
                s.keyboard_focus = 0;
                KEYBOARD_FOCUS_FAST.store(0, Ordering::Release);
            }
            if s.pointer_focus == task_id {
                s.pointer_focus = 0;
            }
        }
    }

    // Free the queue slot.
    if let Some(slot) = find_queue(task_id) {
        let mut queue = TASK_QUEUES[slot].lock();
        queue.active = false;
        queue.task_id = 0;
        queue.events.reset();
        drop(queue);

        // Remove from lookup table.
        let mut alloc = QUEUE_ALLOC.lock();
        if (task_id as usize) < TASK_MAP_SIZE {
            alloc.task_to_slot[task_id as usize] = 0;
        }
        drop(alloc);

        // Release the bitmap slot (lock-free).
        atomic_free_slot(slot);
    }
}
