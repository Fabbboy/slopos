#![feature(restricted_std)]

//! Sysmon selection identity — a selected row must name a *task*, not a
//! position in a list that re-sorts under it.
//!
//! Regression target: selection was a `usize` row index into the sorted order.
//! The table re-sorts on every refresh (CPU% and runtime move constantly), so
//! the highlight stayed on the row while the tasks slid underneath it — and
//! the row context menu and kill dialog acted on whoever had arrived there.

use slopos_userland as _;

use slopos_abi::syscall::types::UserTaskEntry;
use slopos_userland::apps::sysmon::selection::{TaskKey, index_of, key_at_row, row_of};

fn task(pid: u32, started_ms: u64) -> UserTaskEntry {
    UserTaskEntry {
        task_id: pid,
        creation_time_ms: started_ms,
        ..UserTaskEntry::default()
    }
}

/// The bug, stated directly: pick the task on row 1, re-sort, and the
/// selection must follow the task to its new row.
fn test_selection_follows_task_across_resort() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let key = match key_at_row(&tasks, &[0, 1, 2], 0) {
        Some(k) => k,
        None => return false,
    };
    // Row 0 held pid 10; after reversing the order it sits on row 2.
    key.pid == 10 && row_of(key, &tasks, &[2, 1, 0]) == Some(2)
}

/// A task that keeps its row across a re-sort keeps the selection too.
fn test_selection_stable_when_order_unchanged() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let order = [0, 1, 2];
    match key_at_row(&tasks, &order, 1) {
        Some(key) => row_of(key, &tasks, &order) == Some(1),
        None => false,
    }
}

/// Ids recycle. A key names one instantiation, so the same pid with a
/// different start time is a different task and must not inherit the
/// selection — this is what keeps a kill from landing on a stranger.
fn test_recycled_pid_is_not_the_same_task() -> bool {
    let before = [task(13, 100)];
    let after = [task(13, 900)];
    let key = TaskKey::of(&before[0]);
    key.matches(&before[0])
        && !key.matches(&after[0])
        && row_of(key, &after, &[0]).is_none()
        && index_of(key, &after).is_none()
}

/// A selected task that exits stops designating anything, rather than
/// resolving to whichever task now occupies its old row.
fn test_exited_task_has_no_row() -> bool {
    let tasks = [task(10, 1), task(20, 2)];
    let key = TaskKey::of(&tasks[1]);
    row_of(key, &tasks[..1], &[0]).is_none()
}

/// Selecting past the end of the table selects nothing.
fn test_row_beyond_table_selects_nothing() -> bool {
    let tasks = [task(10, 1)];
    key_at_row(&tasks, &[0], 5).is_none()
}

/// `index_of` addresses the task table, `row_of` the display order; on a
/// non-identity sort the two must disagree, or one of them is reading the
/// wrong array.
fn test_table_index_and_display_row_are_distinct() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let order = [2, 0, 1];
    let key = TaskKey::of(&tasks[2]);
    index_of(key, &tasks) == Some(2) && row_of(key, &tasks, &order) == Some(0)
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "selection_follows_task_across_resort",
        test_selection_follows_task_across_resort,
    ),
    (
        "selection_stable_when_order_unchanged",
        test_selection_stable_when_order_unchanged,
    ),
    (
        "recycled_pid_is_not_the_same_task",
        test_recycled_pid_is_not_the_same_task,
    ),
    ("exited_task_has_no_row", test_exited_task_has_no_row),
    (
        "row_beyond_table_selects_nothing",
        test_row_beyond_table_selects_nothing,
    ),
    (
        "table_index_and_display_row_are_distinct",
        test_table_index_and_display_row_are_distinct,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
