//! Queue-slot allocation for the per-task input queues.
//!
//! Task ids are monotonic and never recycled, so the slot table must be keyed
//! by slot rather than by task id: a table indexed by task id carries a
//! ceiling, and a long-lived boot session walks past it and silently stops
//! delivering input to every process created afterwards. These tests drive
//! task ids far above any id a test boot reaches and assert that delivery,
//! read-back and release all still work there.

use slopos_abi::input::{InputEventType, MAX_INPUT_TASKS};
use slopos_testing::{TestResult, fail, pass};

use crate::input_event::{
    input_cleanup_task, input_event_count, input_poll, input_request_close, input_send_configure,
};

/// Comfortably past any id a test boot allocates, and past the ceiling a
/// task-id-indexed table would impose.
const HIGH_TASK_ID: u32 = 1_000_003;

/// Registration, delivery and read-back work for a task id far above the
/// bound any task-id-indexed lookup table could carry.
pub fn test_input_queue_serves_high_task_id() -> TestResult {
    input_cleanup_task(HIGH_TASK_ID);

    if !input_request_close(HIGH_TASK_ID, 42) {
        input_cleanup_task(HIGH_TASK_ID);
        return fail!("no queue could be claimed for task id {}", HIGH_TASK_ID);
    }

    let count = input_event_count(HIGH_TASK_ID);
    let event = input_poll(HIGH_TASK_ID);
    let drained = input_poll(HIGH_TASK_ID);
    input_cleanup_task(HIGH_TASK_ID);

    if count != 1 {
        return fail!("queued event count = {}, want 1", count);
    }
    let event = match event {
        Some(e) => e,
        None => return fail!("queued event was not readable back"),
    };
    if event.event_type != InputEventType::CloseRequest {
        return fail!("read back {:?}, want CloseRequest", event.event_type);
    }
    if event.timestamp_ms != 42 {
        return fail!("read back timestamp {}, want 42", event.timestamp_ms);
    }
    if drained.is_some() {
        return fail!("queue still holds events after being drained");
    }
    pass!()
}

/// Cleanup returns the slot to the pool, so an unbounded run of distinct high
/// task ids never exhausts the [`MAX_INPUT_TASKS`] queues.
pub fn test_input_slot_is_reusable_across_high_task_ids() -> TestResult {
    let rounds = (MAX_INPUT_TASKS * 2) as u32;
    for round in 0..rounds {
        let task_id = HIGH_TASK_ID + 1 + round;
        input_cleanup_task(task_id);

        if !input_send_configure(task_id, 640 + round, 480 + round, round as u64) {
            input_cleanup_task(task_id);
            return fail!(
                "round {}: no queue could be claimed for task id {}",
                round,
                task_id
            );
        }

        let event = input_poll(task_id);
        input_cleanup_task(task_id);

        let event = match event {
            Some(e) => e,
            None => return fail!("round {}: queued event was not readable back", round),
        };
        if event.configure_width() != 640 + round || event.configure_height() != 480 + round {
            return fail!(
                "round {}: read back {}x{}, want {}x{}",
                round,
                event.configure_width(),
                event.configure_height(),
                640 + round,
                480 + round
            );
        }
    }
    pass!()
}

/// A released slot carries none of the previous owner's events into the task
/// that claims it next.
pub fn test_input_slot_release_drops_queued_events() -> TestResult {
    let first = HIGH_TASK_ID + 200;
    let second = HIGH_TASK_ID + 201;
    input_cleanup_task(first);
    input_cleanup_task(second);

    if !input_request_close(first, 7) {
        input_cleanup_task(first);
        return fail!("no queue could be claimed for task id {}", first);
    }
    // Release the slot with the event still queued.
    input_cleanup_task(first);

    if !input_request_close(second, 8) {
        input_cleanup_task(second);
        return fail!("no queue could be claimed for task id {}", second);
    }
    let count = input_event_count(second);
    let event = input_poll(second);
    input_cleanup_task(second);

    if count != 1 {
        return fail!("re-claimed queue holds {} events, want 1", count);
    }
    match event {
        Some(e) if e.timestamp_ms == 8 => pass!(),
        Some(e) => fail!(
            "re-claimed queue yielded the previous owner's event (timestamp {})",
            e.timestamp_ms
        ),
        None => fail!("re-claimed queue yielded no event"),
    }
}

slopos_testing::stest!(
    name = test_input_queue_serves_high_task_id,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_slot_is_reusable_across_high_task_ids,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_slot_release_drops_queued_events,
    suite = input_event
);
