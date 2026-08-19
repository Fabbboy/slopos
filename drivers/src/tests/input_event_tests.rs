//! Queue-slot allocation for the per-task input queues.
//!
//! Task ids are monotonic and never recycled, so the slot table must be keyed
//! by slot rather than by task id: a table indexed by task id carries a ceiling
//! that a long-lived boot walks past, silently dropping input for every process
//! created afterwards. These tests drive ids far above any a test boot reaches.

use slopos_abi::input::{InputEventType, MAX_INPUT_TASKS};
use slopos_testing::{TestResult, fail, pass};

use crate::input_event::{
    input_cleanup_task, input_compositor_task_id, input_event_count, input_poll,
    input_register_compositor, input_request_close, input_send_configure,
};

/// Comfortably past any id a test boot allocates.
const HIGH_TASK_ID: u32 = 1_000_003;

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

/// Cleanup returns the slot to the pool, so a run of distinct task ids never
/// exhausts the [`MAX_INPUT_TASKS`] queues.
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

/// A sink left naming a dead task swallows every key and pointer event for the
/// rest of the boot.
pub fn test_input_sink_is_released_on_exit() -> TestResult {
    let first = HIGH_TASK_ID + 300;
    let second = HIGH_TASK_ID + 301;
    let restore = input_compositor_task_id();

    input_register_compositor(first);
    if input_compositor_task_id() != first {
        input_cleanup_task(first);
        input_register_compositor(restore);
        return fail!("sink did not accept task {}", first);
    }

    input_cleanup_task(first);
    let after_exit = input_compositor_task_id();

    input_register_compositor(second);
    let after_reclaim = input_compositor_task_id();
    input_cleanup_task(second);
    input_register_compositor(restore);

    if after_exit != 0 {
        return fail!("sink still names {} after its holder exited", after_exit);
    }
    if after_reclaim != second {
        return fail!(
            "second task could not claim the sink (got {})",
            after_reclaim
        );
    }
    pass!()
}

pub fn test_input_sink_survives_an_unrelated_exit() -> TestResult {
    let holder = HIGH_TASK_ID + 310;
    let other = HIGH_TASK_ID + 311;
    let restore = input_compositor_task_id();

    input_register_compositor(holder);
    input_cleanup_task(other);
    let still_held = input_compositor_task_id();

    input_cleanup_task(holder);
    input_register_compositor(restore);

    if still_held != holder {
        return fail!(
            "sink lost its holder to an unrelated exit (now {})",
            still_held
        );
    }
    pass!()
}

slopos_testing::stest!(
    name = test_input_queue_serves_high_task_id,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_sink_is_released_on_exit,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_sink_survives_an_unrelated_exit,
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

/// The IRQ-driven routing path never claims a queue slot.
///
/// `input_route_key_full` runs in the PS/2 IRQ handler, where there is no
/// principal to charge for a slot and no errno path to refuse on: routing looks
/// up, and only a syscall claims.
///
/// Focus is set through the raw state rather than `input_set_keyboard_focus`,
/// which legitimately claims.
pub fn test_input_routing_never_claims_a_slot() -> TestResult {
    use crate::input_event::{input_route_key_full, input_set_keyboard_focus};

    const ORPHAN: u32 = 1_000_017;

    input_cleanup_task(ORPHAN);
    let before = input_event_count(ORPHAN);
    if before != 0 {
        return fail!("orphan task starts with {before} events");
    }

    // Give the orphan focus, then take its queue away: focus without a queue
    // is exactly the state an IRQ can observe after a task exits.
    input_set_keyboard_focus(ORPHAN);
    input_cleanup_task(ORPHAN);

    input_route_key_full(0x1E, b'a', 0x04, 'a' as u32, 0, 0, true, 0);

    // A claim would have created a queue and delivered the key into it.
    if input_poll(ORPHAN).is_some() {
        input_cleanup_task(ORPHAN);
        input_set_keyboard_focus(0);
        return fail!("the routing path claimed a queue slot for a task that never asked");
    }

    input_set_keyboard_focus(0);
    input_cleanup_task(ORPHAN);
    pass!()
}

/// The other half of the rule: if nothing claimed at a syscall, the routing
/// path's refusal to claim would lose every event.
pub fn test_input_focus_claims_the_slot_up_front() -> TestResult {
    use crate::input_event::{input_route_key_full, input_set_keyboard_focus};

    const FOCUSED: u32 = 1_000_019;

    input_cleanup_task(FOCUSED);
    input_set_keyboard_focus(FOCUSED);

    input_route_key_full(0x1E, b'a', 0x04, 'a' as u32, 0, 0, true, 0);

    let delivered = input_poll(FOCUSED).is_some();
    input_set_keyboard_focus(0);
    input_cleanup_task(FOCUSED);

    if !delivered {
        return fail!("focus must reserve the queue, or routing loses every event");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_input_routing_never_claims_a_slot,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_focus_claims_the_slot_up_front,
    suite = input_event
);

/// The pointer re-seed self-heal.
///
/// `input_poll_batch` used to re-seed the pointer focus on every call, at
/// frame rate: `if get_pointer_focus() == 0 { set_pointer_focus(self) }`. That
/// re-arm is gone, so the repair has to happen where the loss does — when the
/// focused task is cleaned up. Without it the pointer stays aimed at nothing
/// after any focused window dies, and nothing ever re-aims it.
///
/// A naive "consume a handle" rewrite deletes exactly this; the lost-wake
/// failure class has already bitten this tree twice.
pub fn test_pointer_focus_reseeds_to_the_seat_holder() -> TestResult {
    use crate::input_event::{input_get_pointer_focus, input_set_pointer_focus};
    use slopos_ostd::seat::{self, SeatId, SeatKind};

    const SEAT_HOLDER: u32 = 1_000_023;
    const FOCUSED: u32 = 1_000_029;

    let restore = input_get_pointer_focus();
    input_cleanup_task(SEAT_HOLDER);
    input_cleanup_task(FOCUSED);
    seat::reset_all();

    // The compositor holds the input seat; a different task holds the pointer.
    if seat::acquire(SeatKind::InputSink, SeatId::CompositorPrimary, SEAT_HOLDER).is_err() {
        seat::reset_all();
        return fail!("a reset arbiter must grant a free seat");
    }
    input_set_pointer_focus(FOCUSED, 0);
    if input_get_pointer_focus() != FOCUSED {
        seat::reset_all();
        input_set_pointer_focus(restore, 0);
        return fail!("pointer focus did not take");
    }

    // The focused task dies. The pointer must land back on the seat holder
    // rather than on nothing.
    input_cleanup_task(FOCUSED);
    let reseeded = input_get_pointer_focus();

    // A dying seat holder has no one to re-seed to, so the pointer goes to 0
    // rather than back to itself.
    input_cleanup_task(SEAT_HOLDER);
    let after_holder_death = input_get_pointer_focus();

    seat::reset_all();
    input_set_pointer_focus(restore, 0);
    input_cleanup_task(SEAT_HOLDER);
    input_cleanup_task(FOCUSED);

    if reseeded != SEAT_HOLDER {
        return fail!(
            "pointer focus = {} after the focused task died, want the seat holder {}",
            reseeded,
            SEAT_HOLDER
        );
    }
    if after_holder_death != 0 {
        return fail!(
            "pointer focus = {} after the seat holder itself died, want 0",
            after_holder_death
        );
    }
    pass!()
}

slopos_testing::stest!(
    name = test_pointer_focus_reseeds_to_the_seat_holder,
    suite = input_event
);
