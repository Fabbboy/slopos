//! Input Event Protocol — per-task queues with SeqLock focus tracking.
//!
//! # Locking architecture
//!
//! Focus state (which task receives input) is protected by a [`SeqLock`] —
//! ISR handlers read it lock-free, the compositor writes it rarely.
//!
//! Per-task event queues are individually locked with [`SpinLock`], so
//! event delivery to one task never blocks delivery to another.
//!
//! Resolving a task id to its queue slot is a lock-free scan of
//! [`SLOT_TASK_IDS`], which is what the event-routing path does per event at
//! pointer rates. Claiming and releasing a slot is serialised by
//! [`SLOT_REGISTRY`], touched only on task creation/destruction.

use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};
use slopos_ostd::RingBuffer;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SeqLock, SpinLock};

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
    /// Task that owns this slot, or 0 when the slot is free. Mirrored in
    /// [`SLOT_TASK_IDS`] so routing can find the slot without taking a lock.
    /// This copy is the authoritative one: every operation re-reads it under
    /// the queue lock, because the slot can be released and handed to another
    /// task between a lock-free lookup and the lock that follows it.
    task_id: u32,
    events: RingBuffer<InputEvent, MAX_EVENTS_PER_TASK>,
}

impl TaskEventQueue {
    const fn new() -> Self {
        Self {
            task_id: 0,
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
static TASK_QUEUES: [SpinLock<TaskEventQueue>; MAX_INPUT_TASKS] = [const {
    SpinLock::new(
        TaskEventQueue::new(),
        lock_class!("TASK_QUEUES", LOCK_LEVEL_RESOURCE),
    )
}; MAX_INPUT_TASKS];

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
// Queue Slot Allocation
// =============================================================================

/// Task id occupying each queue slot, or 0 when the slot is free. Mirrors
/// [`TaskEventQueue::task_id`] so that resolving a task to its slot is a
/// lock-free scan of two cache lines rather than a lock acquisition — which
/// matters because the event-routing path resolves once per event and pointer
/// devices report at up to 1000 Hz.
///
/// The array is indexed by *slot*, so it is bounded by the queue pool it
/// describes. Task ids are monotonic and never recycled, so any structure
/// indexed by task id would instead carry a ceiling that a long-lived boot
/// session eventually walks past.
///
/// # Ordering
///
/// A slot's mirror entry is always written **last**, after the queue behind it
/// has been put into the state the entry advertises: on claim, after the queue
/// is bound and drained; on release, after it is unbound and drained. The
/// stores are `Release` and the scans `Acquire`, so a scan that observes an
/// entry also observes the queue state that was published with it.
///
/// The mirror is a hint, never a proof: a scan is not atomic with the lock
/// that follows it, so every operation re-reads `TaskEventQueue::task_id`
/// under the queue lock and abandons the operation if the slot has since been
/// rebound. That check, not the scan, is what keeps one task's events out of
/// another task's queue.
static SLOT_TASK_IDS: [AtomicU32; MAX_INPUT_TASKS] = [const { AtomicU32::new(0) }; MAX_INPUT_TASKS];

/// Serialises claiming and releasing a slot, so a task owns at most one.
///
/// Held only on task creation and destruction, never on the event-routing
/// path. Finding a task's slot needs no lock at all.
///
/// A queue lock is taken while this is held, and never the reverse: nothing
/// acquires a second lock while holding a `TASK_QUEUES` entry, so the
/// dependency graph over these two classes has a single edge and no cycle.
static SLOT_REGISTRY: SpinLock<()> =
    SpinLock::new((), lock_class!("SLOT_REGISTRY", LOCK_LEVEL_REGISTRY));

/// Find the slot a task's queue lives in, without taking any lock.
fn find_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 {
        return None;
    }
    SLOT_TASK_IDS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == task_id)
}

/// Find the slot a task's queue lives in, claiming a free one if it has none.
///
/// Returns `None` only when the task id is invalid or all
/// [`MAX_INPUT_TASKS`] slots are already spoken for.
///
/// # Never from the routing path
///
/// Every caller must be a syscall, a focus change or a registration — a point
/// where a task is *asking* for a queue. `input_route_*` runs in the PS/2 IRQ
/// handler, where there is no principal to charge and no errno to return, so
/// those paths call [`find_queue`] and drop the event when a task has no
/// queue. A claim there would acquire a slot on behalf of a task that never
/// asked, at a point that cannot refuse.
///
/// The queue itself is a fixed `.bss` array, so a claim costs no memory — what
/// it takes is one of [`MAX_INPUT_TASKS`] slots, pre-reserved at its full
/// [`MAX_EVENTS_PER_TASK`] capacity. That is what makes a full queue a bound
/// the owner already paid for, rather than an accounting event at drop time.
fn resolve_queue(task_id: u32) -> Option<usize> {
    if task_id == 0 {
        return None;
    }

    // Fast path: the task already has a slot. No lock, no store.
    if let Some(slot) = find_queue(task_id) {
        return Some(slot);
    }

    // Slow path: claim a slot. The registry lock makes "look, then claim"
    // atomic against a concurrent registration of the same task, which is
    // what stops one task from being handed two slots.
    let _registry = SLOT_REGISTRY.lock();

    // Re-check: another CPU may have claimed a slot for this task while we
    // were on our way to the lock.
    if let Some(slot) = find_queue(task_id) {
        return Some(slot);
    }

    let slot = SLOT_TASK_IDS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == 0)?;

    {
        let mut queue = TASK_QUEUES[slot].lock();
        queue.task_id = task_id;
        queue.events.reset();
    }
    SLOT_TASK_IDS[slot].store(task_id, Ordering::Release);

    Some(slot)
}

// =============================================================================
// Internal: operate on a task's queue
// =============================================================================

/// Run `f` against a task's queue, having confirmed under the queue lock that
/// `slot` still belongs to `task_id`. Returns `None` if it does not.
#[inline]
fn with_queue<R>(slot: usize, task_id: u32, f: impl FnOnce(&mut TaskEventQueue) -> R) -> Option<R> {
    let queue = TASK_QUEUES.get(slot)?;
    let mut queue = queue.lock();
    if queue.task_id != task_id {
        return None;
    }
    Some(f(&mut queue))
}

/// Find a task's queue and run `f` against it. Does not create a queue.
#[inline]
fn with_task_queue<R>(task_id: u32, f: impl FnOnce(&mut TaskEventQueue) -> R) -> Option<R> {
    with_queue(find_queue(task_id)?, task_id, f)
}

#[inline]
fn push_event(slot: usize, task_id: u32, event: InputEvent) {
    with_queue(slot, task_id, |queue| queue.events.push_overwrite(event));
}

// =============================================================================
// Public API - Focus Management (Compositor Operations)
// =============================================================================

#[inline]
pub fn has_keyboard_focus() -> bool {
    KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) != 0
}

pub fn input_set_keyboard_focus(task_id: u32) {
    // Claim the slot here, at the syscall, not on the routing path.
    //
    // `input_route_key_full` runs in the PS/2 IRQ handler, where there is no
    // principal to charge and no errno to return: a slot claimed there is a
    // resource acquired on behalf of a task that never asked, at a point that
    // cannot refuse. Giving focus to a task is the moment it asks, so the
    // queue is reserved with the full capacity it will ever hold and the IRQ
    // path is left with a pure lookup. A queue that is then full is a bound
    // the owner already paid for, and dropping an event stops being an
    // accounting event.
    if task_id != 0 {
        let _ = resolve_queue(task_id);
    }
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
                old_focus,
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
                task_id,
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
        push_event(slot, task_id, InputEvent::close_request(timestamp_ms));
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
        push_event(
            slot,
            task_id,
            InputEvent::configure(width, height, timestamp_ms),
        );
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

/// Route a fully-populated key event to the focused task / compositor.
///
/// Carries both the legacy `(scancode, ascii)` bytes and the canonical
/// `(keycode, codepoint, modifiers, flags)` payload (see
/// [`slopos_abi::input::InputEvent::key_full`]). The keyboard driver builds all
/// fields from `keymap-core`; older consumers that read only
/// `key_scancode()`/`key_ascii()` are unaffected.
#[allow(clippy::too_many_arguments)]
pub fn input_route_key_full(
    scancode: u8,
    ascii: u8,
    keycode: u16,
    codepoint: u32,
    modifiers: u8,
    flags: u8,
    pressed: bool,
    timestamp_ms: u64,
) {
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

    if let Some(slot) = find_queue(target) {
        let event_type = if pressed {
            InputEventType::KeyPress
        } else {
            InputEventType::KeyRelease
        };
        push_event(
            slot,
            target,
            InputEvent::key_full(
                event_type,
                scancode,
                ascii,
                keycode,
                codepoint,
                modifiers,
                flags,
                timestamp_ms,
            ),
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
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_motion(x, y, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }

    let local_x = x - state.window_offset_x;
    let local_y = y - state.window_offset_y;
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
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
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_button(pressed, button, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
            InputEvent::pointer_button(pressed, button, timestamp_ms),
        );
    }
}

pub fn input_route_pointer_axis(axis: u32, value_v120: i32, timestamp_ms: u64) {
    let state = FOCUS.read();
    let comp_id = state.compositor_task_id;
    if comp_id != 0 {
        if let Some(slot) = find_queue(comp_id) {
            push_event(
                slot,
                comp_id,
                InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
            );
        }
        return;
    }

    let focus = state.pointer_focus;
    if focus == 0 {
        return;
    }
    if let Some(slot) = find_queue(focus) {
        push_event(
            slot,
            focus,
            InputEvent::pointer_axis(axis, value_v120, timestamp_ms),
        );
    }
}

// =============================================================================
// Public API - Compositor Registration
// =============================================================================

pub fn input_register_compositor(task_id: u32) {
    // Pre-create the queue: see `input_set_keyboard_focus` for why every
    // claim happens at a syscall and never on the routing path.
    let _ = resolve_queue(task_id);
    // Update focus state to route all raw input to compositor.
    let mut guard = FOCUS.write_lock();
    guard.get_mut().compositor_task_id = task_id;
}

/// The task every raw input event is routed to, or 0 when the sink is free.
pub fn input_compositor_task_id() -> u32 {
    FOCUS.read().compositor_task_id
}

// =============================================================================
// Public API - Client Operations (Syscalls)
// =============================================================================

pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    with_task_queue(task_id, |queue| queue.events.try_pop())?
}

pub fn input_drain_batch(task_id: u32, out_buffer: *mut InputEvent, max_count: usize) -> usize {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let slot = match resolve_queue(task_id) {
        Some(s) => s,
        None => return 0,
    };

    with_queue(slot, task_id, |queue| {
        let mut count = 0;
        while count < max_count {
            if let Some(event) = queue.events.try_pop() {
                slopos_ostd::util::ptr_buf::write_at_index(out_buffer, count, event);
                count += 1;
            } else {
                break;
            }
        }
        count
    })
    .unwrap_or(0)
}

pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    with_task_queue(task_id, |queue| queue.events.peek().copied())?
}

pub fn input_has_events(task_id: u32) -> bool {
    with_task_queue(task_id, |queue| !queue.events.is_empty()).unwrap_or(false)
}

pub fn input_event_count(task_id: u32) -> u32 {
    with_task_queue(task_id, |queue| queue.events.len() as u32).unwrap_or(0)
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

static CLIPBOARD: SpinLock<ClipboardState> = SpinLock::new(
    ClipboardState::new(),
    lock_class!("CLIPBOARD", LOCK_LEVEL_RESOURCE),
);

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
    // Clear focus and the global input sink if this task held either. A stale
    // `compositor_task_id` would keep routing every key and pointer event to a
    // dead task, and `resolve_queue` would keep re-claiming a slot for it.
    {
        let current = FOCUS.read();
        if current.keyboard_focus == task_id
            || current.pointer_focus == task_id
            || current.compositor_task_id == task_id
        {
            let mut guard = FOCUS.write_lock();
            let s = guard.get_mut();
            if s.keyboard_focus == task_id {
                s.keyboard_focus = 0;
                KEYBOARD_FOCUS_FAST.store(0, Ordering::Release);
            }
            if s.pointer_focus == task_id {
                s.pointer_focus = 0;
            }
            if s.compositor_task_id == task_id {
                s.compositor_task_id = 0;
            }
        }
    }

    // Free the queue slot. The registry lock keeps a concurrent claim from
    // taking the slot between the scan and the release.
    let _registry = SLOT_REGISTRY.lock();
    let Some(slot) = find_queue(task_id) else {
        return;
    };
    with_queue(slot, task_id, |queue| {
        queue.task_id = 0;
        queue.events.reset();
    });
    SLOT_TASK_IDS[slot].store(0, Ordering::Release);
}
